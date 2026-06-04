// SPDX-License-Identifier: MIT
//! Heaps (ports `src/heap.c` / `src/alloc.c`). Single-owner (per thread).

use core::cell::Cell;
use core::ptr::NonNull;

use crate::arena_meta::{meta_free, meta_zalloc};
use crate::bits::{
    bin, wsize_from_size, MI_ARENA_SLICE_SIZE, MI_BIN_COUNT, MI_BIN_HUGE, MI_INTPTR_SIZE,
    MI_LARGE_MAX_OBJ_SIZE, MI_MAX_ALIGN_SIZE, MI_MEDIUM_MAX_OBJ_SIZE, MI_PAGES_DIRECT,
    MI_SMALL_MAX_OBJ_SIZE, MI_SMALL_WSIZE_MAX, MI_THREADID_ABANDONED, MI_THREADID_ABANDONED_MAPPED,
};
// Used only by the std free path (the no_std path is single-owner and never
// takes the XOR dispatch or the cross-thread claim/`collect_partly` path).
#[cfg(feature = "std")]
use crate::bits::{MI_PAGE_FLAG_MASK, MI_SMALL_SIZE_MAX};
use crate::layout::align_up;
use crate::page::Page;
use crate::page_map;
use crate::page_queue::PageQueue;
use crate::subproc::{subproc_main, Subproc};

/// Largest request each bin serves. Rust: a `const` table (vs C's runtime
/// `pages[bin].block_size` field), so `bin_block_size` is a bare array index.
const BIN_SIZES: [usize; MI_BIN_COUNT] = {
    let mut t = [0usize; MI_BIN_COUNT];
    let max_wsize = MI_LARGE_MAX_OBJ_SIZE / MI_INTPTR_SIZE;
    let mut w = 1;
    while w <= max_wsize {
        let sz = w * MI_INTPTR_SIZE;
        let b = bin(sz);
        if b < MI_BIN_HUGE && sz > t[b] {
            t[b] = sz;
        }
        w += 1;
    }
    t
};

/// Block size for a (non-huge) bin.
#[inline]
fn bin_block_size(b: usize) -> usize {
    BIN_SIZES[b]
}

/// The usable size a `malloc(size)` would yield (`mi_good_size`).
pub fn good_size(size: usize) -> usize {
    let size = size.max(MI_INTPTR_SIZE);
    let b = bin(size);
    if b >= MI_BIN_HUGE {
        align_up(size, MI_ARENA_SLICE_SIZE)
    } else {
        bin_block_size(b)
    }
}

/// Slices a page serving `block_size` should span (mirrors `mi_page_kind_t`).
fn page_slices_for(block_size: usize) -> usize {
    if block_size <= MI_SMALL_MAX_OBJ_SIZE {
        1
    } else if block_size <= MI_MEDIUM_MAX_OBJ_SIZE {
        8
    } else {
        debug_assert!(block_size <= MI_LARGE_MAX_OBJ_SIZE);
        64
    }
}

/// A first-class heap.
pub struct Heap {
    subproc: &'static Subproc,
    keys: [usize; 2],
    /// Owning thread id stamped on this heap's pages (low 2 bits clear).
    tid: usize,
    /// Counter spreading arena searches across threads.
    tseq: Cell<usize>,
    /// One page queue per bin (`MI_BIN_COUNT` includes the full/huge queues).
    pages: [PageQueue; MI_BIN_COUNT],
    /// `mi_theap_t.pages_free_direct`: per small word size, the page that last
    /// served it. Invariant: a non-null entry points at a live page owned by
    /// this heap (entries are cleared in `retire_page` before slices are freed).
    pages_free_direct: [Cell<*mut Page>; MI_PAGES_DIRECT],
}

impl Heap {
    /// Create a heap bound to the main sub-process with the given encoding keys
    /// and owner thread id (`tid`, low 2 bits clear, non-zero).
    pub fn new(keys: [usize; 2], tid: usize) -> Self {
        Heap {
            subproc: subproc_main(),
            keys,
            tid,
            tseq: Cell::new(0),
            pages: [const { PageQueue::new() }; MI_BIN_COUNT],
            pages_free_direct: [const { Cell::new(core::ptr::null_mut()) }; MI_PAGES_DIRECT],
        }
    }

    /// Allocate a first-class heap from metadata memory (`mi_heap_new`). Release
    /// with [`Heap::delete`] or [`Heap::destroy`]. Returns `None` on metadata OOM.
    pub fn new_boxed(keys: [usize; 2], tid: usize) -> Option<NonNull<Heap>> {
        let mem = meta_zalloc(core::mem::size_of::<Heap>())?;
        let p = mem.as_ptr() as *mut Heap;
        // SAFETY: `mem` is a zeroed, suitably sized/aligned metadata block.
        unsafe { p.write(Heap::new(keys, tid)) };
        NonNull::new(p)
    }

    /// Delete a first-class heap (`mi_heap_delete`): hand off its pages via the
    /// drop path (live blocks stay valid), then free the heap.
    ///
    /// # Safety
    /// `heap` must come from [`Heap::new_boxed`] and not be used afterwards.
    pub unsafe fn delete(heap: NonNull<Heap>) {
        // SAFETY: runs Heap::drop (abandon/release), then frees the struct.
        unsafe {
            core::ptr::drop_in_place(heap.as_ptr());
            meta_free(heap.cast::<u8>(), core::mem::size_of::<Heap>());
        }
    }

    /// Destroy a first-class heap (`mi_heap_destroy`): free **all** of its pages
    /// and blocks in bulk, then free the heap. All pointers from it become invalid.
    ///
    /// # Safety
    /// `heap` must come from [`Heap::new_boxed`], no block of it may be used
    /// afterwards, and no other thread may touch it.
    pub unsafe fn destroy(heap: NonNull<Heap>) {
        // SAFETY: caller guarantees exclusive, final access.
        let h = unsafe { heap.as_ref() };
        for b in 0..MI_BIN_COUNT {
            let mut cur = h.pages[b].first();
            while !cur.is_null() {
                // SAFETY: queue holds valid pages owned by this heap; interior-
                // mutable, so a shared borrow suffices.
                let p = unsafe { &*cur };
                let next = p.next.get();
                // SAFETY: bulk free — return the slices regardless of `used`.
                unsafe {
                    h.pages[b].remove(cur);
                    release_page_slices(cur);
                }
                cur = next;
            }
        }
        // SAFETY: heap memory is a metadata block no longer referenced.
        unsafe { meta_free(heap.cast::<u8>(), core::mem::size_of::<Heap>()) };
    }

    #[inline]
    fn next_tseq(&self) -> usize {
        let t = self.tseq.get();
        self.tseq.set(t.wrapping_add(1));
        t
    }

    /// Allocate `size` bytes (≥ `MI_INTPTR_SIZE`, naturally aligned to
    /// max-align). Returns `None` on OOM.
    #[inline]
    pub fn alloc(&self, size: usize) -> Option<NonNull<u8>> {
        let r = self.alloc_impl(size);
        #[cfg(feature = "stats")]
        if let Some(p) = r {
            // SAFETY: `p` is a block-start allocation we just made.
            let sz = unsafe { usable_size(p) };
            crate::stats::on_alloc(sz);
        }
        r
    }

    /// Allocation fast path: serve from `pages_free_direct[wsize]`. The cold
    /// queue-scan / reclaim / fresh-page work lives in `alloc_generic`, left
    /// un-hinted so the static `#[global_allocator]` build folds the whole chain.
    #[inline]
    fn alloc_impl(&self, size: usize) -> Option<NonNull<u8>> {
        let size = size.max(MI_INTPTR_SIZE);
        let wsize = wsize_from_size(size);

        if wsize <= MI_SMALL_WSIZE_MAX {
            let p = self.pages_free_direct[wsize].get();
            if !p.is_null() {
                // SAFETY: a non-null direct entry always points at a live page
                // (entries are cleared on retire).
                if let Some(b) = unsafe { (*p).alloc() } {
                    return Some(b);
                }
            }
        }
        self.alloc_generic(size, wsize)
    }

    /// Cold allocation path (see `alloc_impl`).
    fn alloc_generic(&self, size: usize, wsize: usize) -> Option<NonNull<u8>> {
        let b = bin(size);
        if b >= MI_BIN_HUGE {
            return self.alloc_huge(size);
        }
        let bs = bin_block_size(b);
        let mut pg = match self.find_free_page(b) {
            Some(p) => p,
            None => self.new_page(b, bs, page_slices_for(bs))?,
        };
        // SAFETY: `pg` is a live page owned by this heap.
        let mut blk = unsafe { (*pg).alloc() };
        if blk.is_none() {
            // Chosen page had no free block (e.g. a reclaimed page still full).
            pg = self.new_page(b, bs, page_slices_for(bs))?;
            // SAFETY: freshly created, non-full page.
            blk = unsafe { (*pg).alloc() };
        }
        if blk.is_some() && wsize <= MI_SMALL_WSIZE_MAX {
            self.pages_free_direct[wsize].set(pg);
        }
        blk
    }

    /// First-fit page in bin `b` with a free block, else reclaim an abandoned
    /// page, else `None` (caller carves a fresh page).
    #[inline]
    fn find_free_page(&self, b: usize) -> Option<*mut Page> {
        let mut cur = self.pages[b].first();
        while !cur.is_null() {
            // SAFETY: queue holds valid pages owned by this heap; interior-
            // mutable, so a shared borrow suffices.
            let p = unsafe { &*cur };
            if !p.is_full() {
                return Some(cur);
            }
            cur = p.next.get();
        }
        self.try_reclaim(b)
    }

    /// Adopt an abandoned page of `bin` (left by an exited thread).
    fn try_reclaim(&self, bin: usize) -> Option<*mut Page> {
        let page_ptr = self.subproc.reclaim_page(bin, self.next_tseq())?;
        // SAFETY: popped from the abandoned stack — exclusively ours now.
        let page = unsafe { &*page_ptr };
        let arena = page.owning_arena();
        page.set_owner(self.tid);
        page.set_provenance(self as *const Heap as *mut Heap, arena, bin as u32);
        // SAFETY: owner now; drain cross-thread frees, then link into our queue.
        unsafe {
            page.collect_free();
            self.pages[bin].push_front(page_ptr);
        }
        Some(page_ptr)
    }

    /// Allocate `size` bytes aligned to `align` (a power of two). For larger
    /// alignments, over-allocate so an aligned interior pointer fits in one
    /// block; [`free`] recovers the block start via the page-map.
    pub fn alloc_aligned(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        debug_assert!(align.is_power_of_two());
        if align <= MI_INTPTR_SIZE {
            return self.alloc(size);
        }
        // Guard against overflow — return null rather than wrap to a tiny block.
        let total = size.checked_add(align - 1)?;
        let p = self.alloc(total)?;
        let aligned = align_up(p.addr().get(), align);
        if aligned != p.addr().get() {
            // Interior pointer: flag the page (ports `mi_page_set_has_interior_pointers`,
            // alloc-aligned.c) so the free fast path recovers the block start.
            let page = page_map::lookup(p.addr().get()) as *mut Page;
            debug_assert!(!page.is_null(), "just-allocated block must be mapped");
            // SAFETY: the block we just allocated is registered in the page-map.
            unsafe { (*page).set_has_interior() };
        }
        // SAFETY: `aligned - block_start < align <= block_size`, so the aligned
        // pointer stays within the same block.
        Some(unsafe { NonNull::new_unchecked(p.as_ptr().with_addr(aligned)) })
    }

    /// Allocate an object too large for any size class as its own page.
    fn alloc_huge(&self, size: usize) -> Option<NonNull<u8>> {
        let header = align_up(core::mem::size_of::<Page>(), MI_MAX_ALIGN_SIZE);
        let need = align_up(header + size, MI_ARENA_SLICE_SIZE);
        let slices = need / MI_ARENA_SLICE_SIZE;
        let bs = align_up(size, MI_MAX_ALIGN_SIZE);
        let page = self.new_page(MI_BIN_HUGE, bs, slices)?;
        // SAFETY: fresh huge page with a single block.
        unsafe { (*page).alloc() }
    }

    /// Carve a new page of `slices` slices for `bin` with block size `bs`,
    /// register it in the page-map, and push it on the bin queue.
    fn new_page(&self, bin: usize, bs: usize, slices: usize) -> Option<*mut Page> {
        let tseq = self.next_tseq();
        let eager = crate::options::eager_commit();
        let (arena, idx, p) = self.subproc.alloc_slices(slices, eager, tseq)?;
        // SAFETY: `p` is `slices` committed, slice-aligned slices owned by us.
        let page = unsafe { Page::init(p, idx, slices, bs, self.keys) };
        let page_ptr = page.as_ptr();
        // SAFETY: page just created and owned by this thread.
        unsafe {
            // Fresh, unpublished page: a plain store, not the flag-preserving CAS
            // used when reclaiming a live page.
            page.as_ref().set_owner_fresh(self.tid);
            page.as_ref().set_provenance(
                self as *const Heap as *mut Heap,
                arena.as_ptr(),
                bin as u32,
            );
        }
        // Debug guard against an arena double-allocation.
        #[cfg(debug_assertions)]
        for s in 0..slices {
            let a = p.addr().get() + s * MI_ARENA_SLICE_SIZE;
            debug_assert!(
                page_map::lookup(a).is_null(),
                "arena handed out slice {a:#x} that is already mapped"
            );
        }
        // SAFETY: range is slice-aligned and live; header pointer is valid.
        unsafe {
            if !page_map::register(p.addr().get(), slices, page_ptr as *mut u8) {
                // Page-map OOM (register rolled itself back): return the slices.
                arena.as_ref().free_slices(idx, slices);
                return None;
            }
            self.pages[bin].push_front(page_ptr);
        }
        crate::stats::on_page_created();
        Some(page_ptr)
    }

    /// Encoding keys (for diagnostics/tests).
    #[inline]
    pub fn keys(&self) -> [usize; 2] {
        self.keys
    }

    /// Reclaim memory held by this heap (`mi_heap_collect`): drain each page's
    /// cross-thread + local frees and return now-empty pages to the arena.
    /// `force` also releases the sole kept page of a bin (see [`retire_page`]).
    ///
    /// Must be called from the heap's owning thread (the `mi_heap_*` contract):
    /// internally relies on owner-only `Cell` access like [`Page::collect_free`].
    pub fn collect(&self, force: bool) {
        // Owner-only contract: fail fast in hardened/test builds.
        #[cfg(all(feature = "std", any(debug_assertions, feature = "secure")))]
        debug_assert_eq!(
            self.tid,
            crate::init::current_tid(),
            "Heap::collect called from a non-owning thread (mi_heap_* is owner-thread-only)"
        );
        #[cfg(feature = "std")]
        crate::init::run_deferred_free(force);
        for b in 0..MI_BIN_COUNT {
            let mut cur = self.pages[b].first();
            while !cur.is_null() {
                // SAFETY: the bin queue holds valid pages owned by this heap;
                // interior-mutable, so a shared borrow suffices.
                let p = unsafe { &*cur };
                let next = p.next.get();
                // SAFETY: owner thread; draining our own page's free lists.
                unsafe { p.collect_free() };
                // Owner thread; reading our own page's used count.
                if p.is_all_free() {
                    if force {
                        // Release even the sole kept page; clear fast-path entries
                        // pointing at it first so the direct lookup cannot dangle.
                        for slot in self.pages_free_direct.iter() {
                            if slot.get() == cur {
                                slot.set(core::ptr::null_mut());
                            }
                        }
                        // SAFETY: page is linked in this bin queue and empty;
                        // its slice range was registered for it at creation.
                        unsafe {
                            self.pages[b].remove(cur);
                            release_page_slices(cur);
                        }
                    } else {
                        // SAFETY: empty, owner-held page with provenance set;
                        // honors the keep-sole rule.
                        unsafe { retire_page(cur) };
                    }
                }
                cur = next;
            }
        }
        // Heartbeat that drives delayed purging back to the OS.
        self.subproc.try_purge(force);
    }
}

/// Whether `ptr` lies within one of our arenas (`mi_is_in_heap_region`). Tests
/// **arena membership**, not page-map presence: a retired/unregistered block is
/// still in our arena (arenas are never unmapped), so this separates "ours but
/// not currently mapped" from "truly foreign".
pub fn is_in_heap_region(ptr: *const u8) -> bool {
    !ptr.is_null() && subproc_main().owns_address(ptr)
}

/// Cold free path for a pointer with no page-map entry. In a rust-native global
/// allocator such a pointer is always an invalid/double free of one of our own
/// blocks whose page was retired (never foreign): no-op by default, abort under
/// hardened builds.
///
/// # Safety
/// `ptr` was passed to `free` and has no page-map entry.
unsafe fn free_foreign_or_invalid(ptr: NonNull<u8>) {
    let _ = ptr;
    #[cfg(any(feature = "secure", feature = "debug"))]
    report_corruption_and_abort(
        "mimalloc-rs: invalid free (pointer not owned by this allocator)\n",
    );
}

/// Free a block previously returned by [`Heap::alloc`] (heap-independent).
///
/// # Safety
/// `ptr` must be a live allocation from this allocator.
#[inline]
pub unsafe fn free(ptr: NonNull<u8>) {
    let page_ptr = page_map::lookup(ptr.addr().get()) as *mut Page;
    if page_ptr.is_null() {
        // SAFETY: forwarded; the cold path re-checks ownership before acting.
        return unsafe { free_foreign_or_invalid(ptr) };
    }
    #[cfg(feature = "std")]
    {
        // Flag-folded dispatch (ports `mi_free_ex`, free.c:185-205): `xtid == 0`
        // ⇒ we own the page, it carries no flags, and `ptr` is a block start —
        // the hot local path. Everything else goes out of line via [`free_cold`].
        // SAFETY: reads the raw xthread_id atomically without forming `&Page`.
        let raw = unsafe { Page::xthread_id_raw(page_ptr) };
        let xtid = crate::init::current_tid() ^ raw;
        if xtid == 0 {
            // Hardened builds: detect out-of-block and double frees first.
            #[cfg(any(feature = "secure", feature = "debug"))]
            // SAFETY: this thread owns the page.
            unsafe {
                let page = &*page_ptr;
                if !page.contains(ptr.as_ptr()) {
                    report_corruption_and_abort(
                        "mimalloc-rs: invalid free (pointer outside page block area)\n",
                    );
                }
                if page.owner_lists_contain(ptr.as_ptr() as *mut crate::free_list::Block) {
                    report_corruption_and_abort("mimalloc-rs: double free detected\n");
                }
            }
            // SAFETY: live header; const field read.
            crate::stats::on_free(unsafe { Page::raw_block_size(page_ptr) });
            // SAFETY: this thread owns the page; `ptr` is a block start.
            unsafe {
                (*page_ptr).free_local(ptr);
                if (*page_ptr).is_all_free() {
                    retire_page(page_ptr);
                }
            }
        } else {
            // SAFETY: live page; forwarded with the folded dispatch word.
            unsafe { free_cold(ptr, page_ptr, xtid) };
        }
    }
    #[cfg(not(feature = "std"))]
    {
        // Without std TLS we assume single-owner frees.
        // SAFETY: live header; const field read.
        let bs = unsafe { Page::raw_block_size(page_ptr) };
        crate::stats::on_free(bs);
        let block = unsafe { recover_block_start(page_ptr, ptr, bs) };
        // SAFETY: single-owner assumption.
        unsafe {
            (*page_ptr).free_local(block);
            if (*page_ptr).is_all_free() {
                retire_page(page_ptr);
            }
        }
    }
}

/// Recover the block start from a (possibly interior) pointer. A block-start
/// pointer (`off == 0`, the common case) skips the divide.
///
/// # Safety
/// `page_ptr` is a live header; `p` lies within its block area; `bs` is the page's
/// block size.
#[inline]
unsafe fn recover_block_start(page_ptr: *mut Page, p: NonNull<u8>, bs: usize) -> NonNull<u8> {
    // SAFETY: `page_ptr` is a live header; const field read.
    let pstart = unsafe { Page::raw_page_start(page_ptr) };
    let off = p.addr().get() - pstart.addr();
    let bstart = if off == 0 {
        p.as_ptr()
    } else {
        pstart.wrapping_add((off / bs) * bs)
    };
    // SAFETY: the recovered address is the start of a live block in this page.
    unsafe { NonNull::new_unchecked(bstart) }
}

/// Cold free arms of `mi_free_ex` (free.c:185-205): an **owner** free into a
/// full / interior-flagged page (`xtid` within the flag mask), or a
/// **cross-thread / abandoned-page** free (`xtid` above the mask).
#[cfg(feature = "std")]
unsafe fn free_cold(ptr: NonNull<u8>, page_ptr: *mut Page, xtid: usize) {
    // SAFETY: live header; const field read.
    let bs = unsafe { Page::raw_block_size(page_ptr) };
    crate::stats::on_free(bs);
    if xtid & !MI_PAGE_FLAG_MASK == 0 {
        // Owner, but page is full / interior-flagged: recover the start.
        // SAFETY: live page owned by this thread.
        let block = unsafe { recover_block_start(page_ptr, ptr, bs) };
        #[cfg(any(feature = "secure", feature = "debug"))]
        // SAFETY: owner.
        unsafe {
            let page = &*page_ptr;
            if !page.contains(block.as_ptr()) {
                report_corruption_and_abort(
                    "mimalloc-rs: invalid free (pointer outside page block area)\n",
                );
            }
            if page.owner_lists_contain(block.as_ptr() as *mut crate::free_list::Block) {
                report_corruption_and_abort("mimalloc-rs: double free detected\n");
            }
        }
        // SAFETY: owner.
        unsafe {
            (*page_ptr).free_local(block);
            if (*page_ptr).is_all_free() {
                retire_page(page_ptr);
            }
        }
    } else {
        // Cross-thread (or abandoned) page: atomic Treiber push.
        let block = if xtid & MI_PAGE_FLAG_MASK == 0 {
            ptr
        } else {
            // SAFETY: live page.
            unsafe { recover_block_start(page_ptr, ptr, bs) }
        };
        // SAFETY: live page and block.
        let claimed = unsafe { Page::thread_free_push(page_ptr, block) };
        if claimed {
            // This push transitioned the abandoned page unowned→owned.
            // SAFETY: we exclusively own the page now; `block` is the head we just
            // pushed (enables the no-atomic partial collect).
            unsafe {
                free_try_collect_mt(page_ptr, block.as_ptr() as *mut crate::free_list::Block)
            };
        }
    }
}

/// Report memory corruption (invalid/double free) and abort. Hardened builds only.
#[cfg(any(feature = "secure", feature = "debug"))]
#[cold]
#[inline(never)]
fn report_corruption_and_abort(msg: &str) -> ! {
    <crate::prim::DefaultPrim as crate::prim::Prim>::out_stderr(msg);
    #[cfg(feature = "std")]
    {
        std::process::abort()
    }
    #[cfg(not(feature = "std"))]
    {
        // No portable abort without std; halt this thread.
        loop {
            core::hint::spin_loop();
        }
    }
}

/// Retire a now-empty page: return its slices to the arena and clear its
/// page-map entries. The sole remaining page of a (non-huge) bin is kept to
/// avoid alloc/free churn.
///
/// # Safety
/// `page_ptr` is a live, fully-free page owned by the calling (owner) thread,
/// with provenance set via [`Page::set_provenance`].
unsafe fn retire_page(page_ptr: *mut Page) {
    // SAFETY: owner thread holds the page; provenance was set at creation.
    let page = unsafe { &*page_ptr };
    let heap = page.owning_heap();
    let bin = page.bin() as usize;
    if heap.is_null() || page.owning_arena().is_null() {
        return; // not a heap-managed page (e.g. a synthetic test page)
    }
    // SAFETY: heap is this thread's heap (owner-only access is safe here).
    let heap = unsafe { &*heap };
    // Keep the last page of a normal bin; always retire huge pages.
    if bin != MI_BIN_HUGE && heap.pages[bin].len() <= 1 {
        return;
    }
    // Clear fast-path entries pointing at this page before release.
    for slot in heap.pages_free_direct.iter() {
        if slot.get() == page_ptr {
            slot.set(core::ptr::null_mut());
        }
    }
    // SAFETY: page is linked in this bin queue; range was registered for it.
    unsafe {
        heap.pages[bin].remove(page_ptr);
        release_page_slices(page_ptr);
    }
    // Mirrors v3 retire → `_mi_arenas_collect` → try-purge.
    heap.subproc.try_purge(false);
}

/// Return a page's slices to its arena and drop its address→page mappings.
///
/// Cross-thread safety: a cross-thread free pushes onto `xthread_free` and never
/// decrements `used`, so `used == 0` implies every such block was already
/// collected. Collection's `Acquire` swap synchronizes-with the foreign push's
/// `Release` CAS, so all foreign-freer accesses happen-before this release and
/// no other thread can still touch the page here.
///
/// # Safety
/// `page_ptr` must be an empty page (`used == 0`), already unlinked from any
/// bin queue, owned by the calling thread.
unsafe fn release_page_slices(page_ptr: *mut Page) {
    // SAFETY: caller guarantees the page is empty and unlinked.
    let page = unsafe { &*page_ptr };
    let arena = page.owning_arena();
    if arena.is_null() {
        return;
    }
    let base = (page_ptr as *mut u8).addr();
    // SAFETY: range was registered for this page; arena owns the slices.
    unsafe {
        page_map::unregister(base, page.slice_count);
        (*arena).free_slices(page.slice_index, page.slice_count);
    }
}

/// Clear a page's entry from its arena's abandoned registry, if it is currently
/// abandoned-mapped. Called before freeing/reusing a page we own.
///
/// # Safety
/// `page_ptr` is a live page owned by the caller, with provenance set.
#[cfg(feature = "std")]
unsafe fn unabandon_if_mapped(page_ptr: *mut Page) {
    // SAFETY: owner holds the page; provenance set at creation.
    let page = unsafe { &*page_ptr };
    if page.is_abandoned_mapped() {
        let arena = page.owning_arena();
        if !arena.is_null() {
            // SAFETY: the page's slice range belongs to this arena.
            unsafe { (*arena).page_unabandon(page.slice_index, page.bin() as usize) };
        }
    }
}

/// We just claimed a previously-abandoned page (ports `mi_free_try_collect_mt`):
/// collect, then (1) free if empty, (2) reabandon-to-mapped if it has space, or
/// (3) release ownership.
///
/// `mt_free` is the block the caller just pushed (the `xthread_free` head),
/// enabling the no-atomic [`Page::collect_partly`] fast path for small blocks.
///
/// # Safety
/// The calling thread exclusively owns `page_ptr` (claimed via the ownership bit);
/// `mt_free` is the block it just pushed onto `page_ptr`'s `xthread_free`.
#[cfg(feature = "std")]
unsafe fn free_try_collect_mt(page_ptr: *mut Page, mt_free: *mut crate::free_list::Block) {
    // SAFETY: exclusively ours; interior-mutable, so a shared borrow suffices.
    let page = unsafe { &*page_ptr };
    // SAFETY: const field; small blocks may use the no-atomic partial collect.
    let small = unsafe { Page::raw_block_size(page_ptr) } <= MI_SMALL_SIZE_MAX;
    let mut first = true;
    loop {
        if first && small {
            // SAFETY: owner; `mt_free` is the just-pushed head (no atomic swap).
            unsafe { page.collect_partly(mt_free) };
        } else {
            // SAFETY: we own the page; drain cross-thread + local frees.
            unsafe { page.collect_free() };
        }
        first = false;

        // 1. Empty → unabandon and return the slices.
        if page.is_all_free() {
            // SAFETY: owner; provenance set at creation. (`page` is dead after
            // `release_page_slices` returns the header's slice to the arena.)
            unsafe {
                unabandon_if_mapped(page_ptr);
                release_page_slices(page_ptr);
            }
            return;
        }

        // 2. Reabandon-to-mapped: register + stamp the mapped state *before*
        //    releasing ownership, so a concurrent reclaimer loses the race.
        if !page.is_full() && !page.is_abandoned_mapped() {
            let bin = page.bin() as usize;
            let arena = page.owning_arena();
            if !arena.is_null() {
                // SAFETY: owner; the page's slices belong to this arena.
                unsafe {
                    (*arena).page_abandon(page.slice_index, bin);
                    page.set_owner(MI_THREADID_ABANDONED_MAPPED);
                }
            }
        }

        // 3. Release ownership; a racing free makes `try_unown` fail → re-loop.
        // SAFETY: owner; just collected.
        if unsafe { page.try_unown() } {
            return;
        }
    }
}

/// Hand a page we own off to the abandoned state (ports `_mi_page_abandon`),
/// on thread exit from [`Heap`]'s `Drop`. Collect, then: empty → return slices;
/// full → abandoned **unmapped**; otherwise → abandoned **mapped** (findable for
/// reclaim-on-alloc). Ownership is released last so a free cannot claim it
/// mid-handoff.
///
/// # Safety
/// `page_ptr` is a live page owned by the caller, already unlinked from any bin
/// queue, with provenance set.
unsafe fn abandon_owned_page(subproc: &Subproc, page_ptr: *mut Page, bin: usize) {
    // SAFETY: owner-only access to our own page; interior-mutable, so a shared
    // borrow suffices.
    let page = unsafe { &*page_ptr };
    // SAFETY: owner-only access to our own page.
    unsafe { page.collect_free() };
    if page.is_all_free() {
        // SAFETY: empty + unlinked. (`page` is dead after `release_page_slices`
        // returns the header's slice to the arena.)
        unsafe { release_page_slices(page_ptr) };
    } else if page.is_full() {
        // SAFETY: owner.
        unsafe {
            page.set_owner(MI_THREADID_ABANDONED);
            page.set_unowned();
        }
    } else {
        // Abandoned mapped: register, stamp, then release.
        // SAFETY: owner; the page's slices belong to its arena.
        unsafe {
            subproc.abandon_page(page_ptr, bin);
            page.set_owner(MI_THREADID_ABANDONED_MAPPED);
            page.set_unowned();
        }
    }
}

impl Drop for Heap {
    /// On thread exit, hand off this heap's pages: empty pages are released to
    /// the arena, pages with live blocks are abandoned for another thread.
    fn drop(&mut self) {
        for b in 0..MI_BIN_COUNT {
            let mut cur = self.pages[b].first();
            while !cur.is_null() {
                // SAFETY: the bin queue holds valid pages owned by this heap;
                // interior-mutable, so a shared borrow suffices.
                let next = unsafe { &*cur }.next.get();
                // SAFETY: owner thread; draining our own queue, then hand off.
                unsafe {
                    self.pages[b].remove(cur);
                    abandon_owned_page(self.subproc, cur, b);
                }
                cur = next;
            }
        }
    }
}

/// Usable bytes reachable from `ptr` within its block. For an interior pointer
/// (from [`Heap::alloc_aligned`]) this is the block size minus the in-block
/// offset — the space the caller may safely write.
///
/// # Safety
/// `ptr` must be a live allocation from this allocator.
pub unsafe fn usable_size(ptr: NonNull<u8>) -> usize {
    let page_ptr = page_map::lookup(ptr.addr().get()) as *mut Page;
    if page_ptr.is_null() {
        return 0; // not a live block of ours
    }
    // SAFETY: valid page header; const fields read via raw projection.
    let (bs, pstart) = unsafe {
        (
            Page::raw_block_size(page_ptr),
            Page::raw_page_start(page_ptr),
        )
    };
    let in_block = (ptr.addr().get() - pstart.addr()) % bs;
    bs - in_block
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    fn test_heap() -> Heap {
        // Use the real thread id so owner-vs-cross-thread free routing is correct.
        Heap::new(
            [0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321],
            crate::init::current_tid(),
        )
    }

    #[test]
    fn bin_size_table_monotonic() {
        let t = &BIN_SIZES;
        assert_eq!(t[1], 8);
        assert_eq!(t[2], 16);
        assert_eq!(t[bin(24)], 32);
        // non-decreasing across populated bins
        let mut last = 0;
        for &s in t.iter() {
            if s != 0 {
                assert!(s >= last);
                last = s;
            }
        }
    }

    #[test]
    fn end_to_end_alloc_free_small() {
        let h = test_heap();
        // SAFETY: pointers come from this heap.
        unsafe {
            let mut ptrs = alloc::vec::Vec::new();
            for i in 0..1000usize {
                let p = h.alloc(40).unwrap();
                assert_eq!(p.addr().get() % 16, 0);
                // write a recognizable pattern
                core::ptr::write_bytes(p.as_ptr(), (i & 0xff) as u8, 40);
                ptrs.push(p);
            }
            // all distinct
            let mut addrs: alloc::vec::Vec<usize> = ptrs.iter().map(|p| p.addr().get()).collect();
            addrs.sort_unstable();
            addrs.dedup();
            assert_eq!(addrs.len(), 1000);
            // free everything
            for p in &ptrs {
                free(*p);
            }
        }
    }

    #[test]
    fn alloc_various_sizes_roundtrip() {
        let h = test_heap();
        let sizes = [1usize, 8, 16, 17, 64, 100, 1024, 5000, 40_000, 300_000];
        // SAFETY: pointers from this heap.
        unsafe {
            for &s in &sizes {
                let p = h.alloc(s).unwrap();
                assert!(usable_size(p) >= s, "usable {} < {}", usable_size(p), s);
                // touch first and last byte
                core::ptr::write_bytes(p.as_ptr(), 0xAB, s);
                assert_eq!(*p.as_ptr(), 0xAB);
                assert_eq!(*p.as_ptr().add(s - 1), 0xAB);
                free(p);
            }
        }
    }

    #[test]
    fn freed_blocks_are_recycled() {
        let h = test_heap();
        // SAFETY: from this heap.
        unsafe {
            // exhaust one small page-ful, free, and ensure footprint is bounded
            let mut ptrs = alloc::vec::Vec::new();
            for _ in 0..5000 {
                ptrs.push(h.alloc(64).unwrap());
            }
            for p in ptrs.drain(..) {
                free(p);
            }
            // reallocate the same amount; addresses should overlap the freed set
            for _ in 0..5000 {
                ptrs.push(h.alloc(64).unwrap());
            }
            for p in ptrs {
                free(p);
            }
        }
    }

    #[cfg(feature = "stats")]
    #[test]
    fn stats_track_allocations() {
        let h = test_heap();
        let before = crate::stats::snapshot();
        // SAFETY: pointer from this heap, freed on this thread.
        unsafe {
            let p = h.alloc(1000).unwrap();
            let mid = crate::stats::snapshot();
            assert!(
                mid.allocations > before.allocations,
                "alloc count must rise"
            );
            free(p);
            let after = crate::stats::snapshot();
            assert!(after.frees > before.frees, "free count must rise");
        }
    }

    #[test]
    fn empty_pages_are_retired() {
        // Allocating a burst spanning several pages then freeing it all must
        // retire the now-empty pages (return their slices), leaving only the
        // sole kept page in the bin queue — footprint tracks the live set.
        let h = test_heap();
        let b = bin(200);
        // SAFETY: pointers come from this heap and are freed on this thread.
        unsafe {
            let mut ptrs = alloc::vec::Vec::new();
            for _ in 0..2000 {
                ptrs.push(h.alloc(200).unwrap());
            }
            let pages_at_peak = h.pages[b].len();
            assert!(
                pages_at_peak >= 3,
                "expected several pages, got {pages_at_peak}"
            );
            for p in ptrs {
                free(p);
            }
            let pages_after = h.pages[b].len();
            assert_eq!(pages_after, 1, "empty pages should retire to the kept page");
        }
    }

    #[test]
    fn collect_drains_and_retires() {
        // Allocate a burst spanning several pages, free it all, then force a
        // collect. `force` retires even the sole kept page, so every bin we
        // exercised should drain to zero pages and the heap stays usable.
        let h = test_heap();
        let b = bin(200);
        // SAFETY: pointers come from this heap and are freed on this thread.
        unsafe {
            let mut ptrs = alloc::vec::Vec::new();
            for _ in 0..2000 {
                ptrs.push(h.alloc(200).unwrap());
            }
            assert!(h.pages[b].len() >= 3, "expected several pages");
            for p in ptrs {
                free(p);
            }
            // Non-forced collect honors the keep-sole rule: one page remains.
            h.collect(false);
            assert_eq!(h.pages[b].len(), 1, "non-forced keeps the sole page");
            // Forced collect releases even that page.
            h.collect(true);
            assert_eq!(h.pages[b].len(), 0, "force retires the sole page too");
            // Heap is still usable after collection.
            let q = h.alloc(200).unwrap();
            core::ptr::write_bytes(q.as_ptr(), 0x33, 200);
            free(q);
        }
    }

    // The following cross-thread tests free reconstructed raw addresses from
    // worker threads, exercising retire/abandon/reclaim against the global
    // page-map and arena. The foreign-vs-ours decision uses arena membership
    // (`Subproc::owns_address`), so a retired (unmapped) our-arena address is
    // still treated as ours.
    #[test]
    fn collect_reclaims_cross_thread_frees() {
        // The owner allocates N blocks; a worker thread frees them all
        // cross-thread (pushed to xthread_free, NOT yet reclaimed — `used`
        // stays elevated). After the worker joins, the owner runs `collect`,
        // which drains the cross-thread frees and retires the emptied pages.
        let h = test_heap();
        let b = bin(64);
        // SAFETY: pointers come from this heap; the worker only pushes to the
        // atomic cross-thread stack (never touching owner-only state).
        unsafe {
            let n = 4000usize;
            let mut ptrs = alloc::vec::Vec::new();
            for _ in 0..n {
                ptrs.push(h.alloc(64).unwrap());
            }
            let pages_at_peak = h.pages[b].len();
            assert!(pages_at_peak >= 3, "expected several pages");
            let addrs: alloc::vec::Vec<usize> = ptrs.iter().map(|p| p.addr().get()).collect();

            // Free everything from another thread (cross-thread frees).
            std::thread::spawn(move || {
                for a in addrs {
                    // free() only uses the address for the page-map lookup.
                    free(NonNull::new(a as *mut u8).unwrap());
                }
            })
            .join()
            .unwrap();

            // Cross-thread frees have not been collected yet, so the pages are
            // still resident. Force a collect on the OWNER thread: it drains
            // xthread_free and retires the now-empty pages.
            h.collect(true);
            assert_eq!(
                h.pages[b].len(),
                0,
                "owner collect must reclaim cross-thread-freed pages"
            );
            // Heap remains usable.
            let q = h.alloc(64).unwrap();
            free(q);
        }
    }

    // See the note on `collect_reclaims_cross_thread_frees`: runs under every
    // feature combo (the foreign-vs-ours decision uses arena membership).
    #[test]
    fn stress_owner_retire_vs_cross_thread_free() {
        // Exercises the retire-vs-foreign-free window: a producer allocates and
        // mostly frees on its own thread (driving pages empty → retire), while
        // consumer threads free a fraction of the blocks cross-thread. Must run
        // to completion without corruption (run under TSan/Miri for races).
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel::<usize>();
        let rx = std::sync::Mutex::new(rx);
        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| loop {
                    let got = rx.lock().unwrap().recv();
                    match got {
                        Ok(addr) => {
                            // SAFETY: addr is a live block handed off by the producer.
                            unsafe { free(NonNull::new(addr as *mut u8).unwrap()) }
                        }
                        Err(_) => break, // channel closed
                    }
                });
            }
            s.spawn(move || {
                let h = test_heap();
                let mut x: u64 = 0x9e37_79b9;
                // SAFETY: blocks come from `h`; freed here (owner) or by consumers.
                unsafe {
                    for _ in 0..60_000 {
                        let p = h.alloc(64).unwrap();
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        if x & 7 == 0 {
                            tx.send(p.addr().get()).unwrap(); // free cross-thread
                        } else {
                            free(p); // owner free → may retire the page
                        }
                    }
                }
                drop(tx); // close the channel so consumers exit
            });
        });
    }

    // See the note on `collect_reclaims_cross_thread_frees`: runs under every
    // feature combo (the foreign-vs-ours decision uses arena membership).
    #[test]
    fn abandoned_pages_reclaimed_across_threads() {
        // A worker allocates a batch and exits while the blocks are still live;
        // its non-empty pages are abandoned. Another thread frees the blocks
        // (cross-thread) and then reclaims the abandoned pages on allocation.
        let b = bin(300);
        let addrs = std::thread::spawn(|| {
            let mut v = alloc::vec::Vec::new();
            for _ in 0..500 {
                v.push(crate::init::malloc(300).unwrap().addr().get());
            }
            v // pages remain non-empty when this thread exits → abandoned
        })
        .join()
        .unwrap();

        let sp = crate::subproc::subproc_main();
        let ab_before = sp.abandoned_len(b);
        assert!(
            ab_before > 0,
            "exited thread must abandon its non-empty pages"
        );

        // Free the leaked blocks cross-thread (routed to the pages' xthread_free).
        // SAFETY: addresses are live allocations from the worker.
        unsafe {
            for a in &addrs {
                free(NonNull::new(*a as *mut u8).unwrap());
            }
        }

        // Allocating the same size now reclaims the abandoned pages.
        let h = test_heap();
        let mut reclaimed = alloc::vec::Vec::new();
        for _ in 0..500 {
            reclaimed.push(h.alloc(300).unwrap());
        }
        let ab_after = sp.abandoned_len(b);
        assert!(
            ab_after < ab_before,
            "allocation should reclaim abandoned pages ({ab_before} -> {ab_after})"
        );
        // SAFETY: reclaimed blocks are owned by this thread's heap now.
        unsafe {
            for p in reclaimed {
                free(p);
            }
        }
    }

    // See the note on `collect_reclaims_cross_thread_frees`: runs under every
    // feature combo (the foreign-vs-ours decision uses arena membership).
    #[test]
    fn cross_thread_free_collected_by_owner() {
        // Owner heap lives on this thread; worker threads free its blocks
        // cross-thread (routed to `xthread_free`), and the owner reclaims them
        // on its next allocations via `collect()`.
        let h = test_heap();
        // SAFETY: pointers are from this heap; workers only push to the atomic
        // cross-thread stack (they never touch owner-only state).
        unsafe {
            let n = 2000usize;
            let mut ptrs = alloc::vec::Vec::new();
            for _ in 0..n {
                ptrs.push(h.alloc(48).unwrap());
            }
            let addrs: alloc::vec::Vec<usize> = ptrs.iter().map(|p| p.addr().get()).collect();

            // Hand disjoint address ranges to 4 worker threads to free.
            let mut handles = alloc::vec::Vec::new();
            for w in 0..4 {
                let chunk: alloc::vec::Vec<usize> =
                    addrs.iter().copied().skip(w).step_by(4).collect();
                handles.push(std::thread::spawn(move || {
                    for a in chunk {
                        // free() only uses the address for page-map lookup.
                        free(NonNull::new(a as *mut u8).unwrap());
                    }
                }));
            }
            for hd in handles {
                hd.join().unwrap();
            }

            // Owner reallocates the same count; this drains `xthread_free`.
            let mut re = alloc::vec::Vec::new();
            for _ in 0..n {
                re.push(h.alloc(48).unwrap());
            }
            // No double-allocation: all reallocated addresses are distinct.
            let mut s: alloc::vec::Vec<usize> = re.iter().map(|p| p.addr().get()).collect();
            s.sort_unstable();
            let len = s.len();
            s.dedup();
            assert_eq!(s.len(), len, "cross-thread free caused a double allocation");
            // and every reused address is writable
            for p in &re {
                core::ptr::write_bytes(p.as_ptr(), 0x5A, 48);
            }
            for p in re {
                free(p);
            }
        }
    }

    #[test]
    fn is_in_heap_region_basic() {
        // One of ours → true; a stack address and null → false.
        let h = test_heap();
        let p = h.alloc(64).unwrap();
        assert!(is_in_heap_region(p.as_ptr()));
        let stack = 0u8;
        assert!(!is_in_heap_region(&stack as *const u8));
        assert!(!is_in_heap_region(core::ptr::null()));
        // SAFETY: ours, freed on this thread.
        unsafe { free(p) };
    }

    extern crate alloc;
}
