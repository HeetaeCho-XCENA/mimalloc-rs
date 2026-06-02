// SPDX-License-Identifier: MIT
//! Heaps (ports the allocation core of `src/heap.c` / `src/alloc.c`).
//!
//! A heap owns one [`PageQueue`] per size-class bin and turns size requests into
//! blocks: pick the bin, find a page with a free block (or carve a new page from
//! an arena via the [`crate::subproc`]), and pop a block. Freeing is heap
//! independent — it finds the owning page through the [`crate::page_map`].
//!
//! The heap is single-owner (per thread). It has a `pages_free_direct` fast
//! array (skip the bin-queue scan for small sizes), retires empty pages to the
//! arena, and on thread exit abandons/releases its pages (see [`crate::page`]
//! and [`crate::subproc`]). The `mi_theap_t`/`tld` split remains follow-up work.

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

/// Canonical block size for each bin (the largest request the bin serves),
/// evaluated at compile time. C reads `pages[bin].block_size` (a plain field);
/// computing this table as a `const` makes `bin_block_size` a bare array index
/// with no runtime initialization atomic on the direct-miss alloc path.
const BIN_SIZES: [usize; MI_BIN_COUNT] = {
    let mut t = [0usize; MI_BIN_COUNT];
    // Scan every word size up to the largest non-huge object and record the
    // maximum byte size landing in each bin.
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

/// The usable size a `malloc(size)` would yield (`mi_good_size`): the block
/// size of the bin the request maps to (huge requests round up to a slice).
pub fn good_size(size: usize) -> usize {
    let size = size.max(MI_INTPTR_SIZE);
    let b = bin(size);
    if b >= MI_BIN_HUGE {
        align_up(size, MI_ARENA_SLICE_SIZE)
    } else {
        bin_block_size(b)
    }
}

/// How many 64 KiB slices a page serving `block_size` blocks should span
/// (small ⇒ 1, medium ⇒ 8, large ⇒ 64), mirroring `mi_page_kind_t`.
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
    /// Fast-path lookup (`mi_theap_t.pages_free_direct`): for each small word
    /// size, the page that most recently served it. The malloc fast path tries
    /// this page directly, skipping the bin-queue scan.
    ///
    /// Invariant: a non-null entry points at a live page owned by this heap.
    /// Maintained because (a) the array is per-heap and only ever stores pages
    /// this heap allocated from, (b) `retire_page` clears matching entries
    /// before releasing slices, and (c) `Heap::drop` releases/abandons pages and
    /// then the array itself is dropped — so an entry can never outlive its page.
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

    /// Allocate a first-class heap from metadata memory (mirrors `mi_heap_new`).
    /// The returned heap is owned by `tid` and must be released with
    /// [`Heap::delete`] (keeps live blocks valid) or [`Heap::destroy`] (frees
    /// all its blocks at once). Returns `None` on metadata OOM.
    pub fn new_boxed(keys: [usize; 2], tid: usize) -> Option<NonNull<Heap>> {
        let mem = meta_zalloc(core::mem::size_of::<Heap>())?;
        let p = mem.as_ptr() as *mut Heap;
        // SAFETY: `mem` is a zeroed, suitably sized/aligned metadata block.
        unsafe { p.write(Heap::new(keys, tid)) };
        NonNull::new(p)
    }

    /// Delete a first-class heap (mirrors `mi_heap_delete`): hands off its pages
    /// via the normal drop path (empty pages released, non-empty abandoned so
    /// outstanding blocks stay valid and reclaimable), then frees the heap.
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

    /// Destroy a first-class heap (mirrors `mi_heap_destroy`): free **all** of
    /// its pages and their blocks in bulk, then free the heap. All pointers
    /// allocated from this heap become invalid.
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
                // SAFETY: queue holds valid pages owned by this heap.
                let next = unsafe { (*cur).next.get() };
                // SAFETY: bulk free — return the slices regardless of `used`.
                unsafe {
                    h.pages[b].remove(cur);
                    release_page_slices(cur);
                }
                cur = next;
            }
        }
        // Free the heap struct itself (no Drop: its pages are already released).
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
        // Account by block size (matches `free`); only compiled under `stats`.
        #[cfg(feature = "stats")]
        if let Some(p) = r {
            // SAFETY: `p` is a block-start allocation we just made.
            let sz = unsafe { usable_size(p) };
            crate::stats::on_alloc(sz);
        }
        r
    }

    /// Allocation fast path: serve from the page that last served this word size
    /// (`pages_free_direct`), which usually still has a free block. The cold
    /// queue-scan / reclaim / fresh-page work lives in `alloc_generic`.
    ///
    /// `alloc_generic` is marked `#[cold]` **only in the preload `cdylib` build**
    /// (`cfg(override_export)`): there the fast shell must stay small so it inlines
    /// across the export-symbol boundary into `malloc`/`operator new` (mirroring
    /// C's force-inlined `mi_page_malloc_zero` over the noinline `_mi_malloc_generic`).
    /// In a statically-linked `#[global_allocator]` build the caller sees the whole
    /// chain and inlines holistically, so we leave `alloc_generic` un-hinted and let
    /// the optimizer fold it back in — forcing it out of line there measurably
    /// regresses the small-alloc hot path (phase 1).
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

    /// Cold allocation path: no direct page was available — map the size to its
    /// bin and scan the bin queue, reclaim an abandoned page, or carve a fresh
    /// one. (`#[cold]` only in the preload cdylib; see `alloc_impl`.)
    #[cfg_attr(override_export, cold)]
    fn alloc_generic(&self, size: usize, wsize: usize) -> Option<NonNull<u8>> {
        let b = bin(size);
        if b >= MI_BIN_HUGE {
            return self.alloc_huge(size);
        }
        let bs = bin_block_size(b);
        // Pick the page that will serve this request, then record it for the
        // fast path. Scan the bin queue, else reclaim an abandoned page, else
        // carve a fresh one.
        let mut pg = {
            let mut found = core::ptr::null_mut();
            let mut cur = self.pages[b].first();
            while !cur.is_null() {
                // SAFETY: queue holds valid pages owned by this heap.
                if !unsafe { (*cur).is_full() } {
                    found = cur;
                    break;
                }
                cur = unsafe { (*cur).next.get() };
            }
            if found.is_null() {
                found = self.try_reclaim(b).unwrap_or(core::ptr::null_mut());
            }
            if found.is_null() {
                found = self.new_page(b, bs, page_slices_for(bs))?;
            }
            found
        };
        // SAFETY: `pg` is a live page owned by this heap.
        let mut blk = unsafe { (*pg).alloc() };
        if blk.is_none() {
            // The chosen page had no free block (e.g. a reclaimed page whose
            // blocks are all still live) — carve a fresh page instead.
            pg = self.new_page(b, bs, page_slices_for(bs))?;
            // SAFETY: freshly created, non-full page.
            blk = unsafe { (*pg).alloc() };
        }
        if blk.is_some() && wsize <= MI_SMALL_WSIZE_MAX {
            self.pages_free_direct[wsize].set(pg);
        }
        blk
    }

    /// Adopt an abandoned page of `bin` (left by an exited thread): claim
    /// ownership, drain the cross-thread frees that accumulated while it was
    /// abandoned, re-home it into this heap, and return it.
    fn try_reclaim(&self, bin: usize) -> Option<*mut Page> {
        let page = self.subproc.reclaim_page(bin, self.next_tseq())?;
        // SAFETY: popped from the abandoned stack — exclusively ours now.
        unsafe {
            let arena = (*page).owning_arena();
            (*page).set_owner(self.tid);
            (*page).set_provenance(self as *const Heap as *mut Heap, arena, bin as u32);
            // Collect blocks freed cross-thread while the page was abandoned.
            (*page).collect_free();
            self.pages[bin].push_front(page);
        }
        Some(page)
    }

    /// Allocate `size` bytes aligned to `align` (a power of two).
    ///
    /// For `align <= MI_INTPTR_SIZE` every block is already suitably aligned.
    /// For larger alignments we over-allocate so an aligned pointer fits inside
    /// one block; [`free`] recovers the block start from the interior pointer
    /// via the page-map, so no extra bookkeeping is needed.
    pub fn alloc_aligned(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        debug_assert!(align.is_power_of_two());
        if align <= MI_INTPTR_SIZE {
            return self.alloc(size);
        }
        // Guard the over-allocation against overflow (e.g. a huge `size` with a
        // large alignment) — return null rather than wrapping to a tiny block.
        let total = size.checked_add(align - 1)?;
        let p = self.alloc(total)?;
        let aligned = align_up(p.addr().get(), align);
        if aligned != p.addr().get() {
            // We are handing out an *interior* pointer. Flag the page so the free
            // fast path recovers the block start instead of assuming a block-start
            // pointer (ports `mi_page_set_has_interior_pointers` in
            // `alloc-aligned.c`). Only the rare large-alignment path pays the
            // page-map lookup.
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
        // One block occupying the whole page area.
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
        // `eager_commit` (option) controls whether a freshly reserved arena is
        // committed up front or committed per-slice on demand (lower RSS).
        let eager = crate::options::eager_commit();
        let (arena, idx, p) = self.subproc.alloc_slices(slices, eager, tseq)?;
        // SAFETY: `p` is `slices` committed, slice-aligned slices owned by us.
        let page = unsafe { Page::init(p, idx, slices, bs, self.keys) };
        let page_ptr = page.as_ptr();
        // Stamp ownership so cross-thread frees route to `xthread_free`, and
        // record heap/arena/bin so the page can be retired when it empties.
        // SAFETY: page just created and owned by this thread.
        unsafe {
            // Fresh, unpublished page (flags are 0, no other thread can see it):
            // a plain store, not the flag-preserving CAS used when reclaiming a
            // live page — the per-page creation cost shows up directly on the
            // huge workload (one page per allocation).
            page.as_ref().set_owner_fresh(self.tid);
            page.as_ref().set_provenance(
                self as *const Heap as *mut Heap,
                arena.as_ptr(),
                bin as u32,
            );
        }
        // Map every slice of the page back to its header.
        // Debug guard: a freshly carved slice run must not already be mapped to
        // another page (would indicate an arena double-allocation).
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
                // OOM in the page-map (register rolled back its own entries):
                // return the slices to the arena so nothing leaks.
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

    /// Reclaim memory held by this heap (mirrors `mi_heap_collect`): walk every
    /// bin queue, drain each page's pending cross-thread + local frees, and
    /// return now-empty pages to the arena so footprint tracks the live set.
    ///
    /// When `force` is set, collection is more aggressive: even the sole kept
    /// page of a bin (normally retained to avoid rebuild churn — see
    /// [`retire_page`]) is released. Otherwise the keep-sole rule is honored.
    ///
    /// This is a safe `&self` method but internally relies on owner-only access
    /// to each page's local free lists and bin queues, exactly like
    /// [`Page::collect_free`]. Per the documented `mi_heap_*` single-thread
    /// contract it must be called from the heap's owning thread; calling it from
    /// any other thread is a logic error (the blocks freed cross-thread are still
    /// reclaimed safely, but the owner-only `Cell` accesses are not synchronized).
    ///
    /// Unlike the C `mi_heap_collect`, this does not (yet) purge empty arena
    /// ranges back to the OS or merge per-thread statistics — `force` is more
    /// aggressive only about releasing pages, not about driving down RSS. Those
    /// are tracked as follow-up work (see the module header).
    pub fn collect(&self, force: bool) {
        // Owner-only contract: fail fast in hardened/test builds if a non-owning
        // thread calls in (matches the C reference's owner-tid guard).
        #[cfg(all(feature = "std", any(debug_assertions, feature = "secure")))]
        debug_assert_eq!(
            self.tid,
            crate::init::current_tid(),
            "Heap::collect called from a non-owning thread (mi_heap_* is owner-thread-only)"
        );
        // Fire any registered deferred-free callback (our heartbeat point).
        #[cfg(feature = "std")]
        crate::init::run_deferred_free(force);
        for b in 0..MI_BIN_COUNT {
            let mut cur = self.pages[b].first();
            while !cur.is_null() {
                // SAFETY: the bin queue holds valid pages owned by this heap.
                let next = unsafe { (*cur).next.get() };
                // SAFETY: owner thread; draining our own page's free lists.
                unsafe { (*cur).collect_free() };
                // SAFETY: owner thread; reading our own page's used count.
                if unsafe { (*cur).is_all_free() } {
                    if force {
                        // Aggressive: release even the sole kept page. Clear any
                        // fast-path entries pointing at it first so the direct
                        // lookup can never dangle (the same guard `retire_page`
                        // applies). Every page in `self.pages[b]` is heap-managed.
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
        // Return any due (and, with `force`, all) freed-but-still-resident arena
        // slices to the OS. This is the heartbeat that drives delayed purging.
        self.subproc.try_purge(force);
    }
}

/// Whether `ptr` points into a region this allocator manages (i.e. it lies
/// within one of our arenas). Foreign pointers (system malloc, the dynamic
/// linker, TLS, etc.) return false. This is the basis for the override fallback
/// and for `mi_is_in_heap_region`.
///
/// This tests **arena membership**, not page-map presence (mirroring the C
/// `mi_is_in_heap_region`, which tests arena/region membership). A page-map
/// lookup is null for both genuinely foreign pointers *and* our own pointers
/// whose page was retired/unregistered (a double-free, a free racing a retire,
/// or freeing an already-reclaimed block). Such a pointer is still inside our
/// arena — arenas are never unmapped back to the OS — so handing it to the
/// system allocator (under `override`) would abort. Arena membership separates
/// "ours but not currently mapped" from "truly foreign".
pub fn is_in_heap_region(ptr: *const u8) -> bool {
    !ptr.is_null() && subproc_main().owns_address(ptr)
}

/// Cold free path for a pointer with no page-map entry: it is either genuinely
/// foreign (system malloc, the linker, TLS) or one of *ours* whose page was
/// retired/unregistered (a double-free, a free racing a concurrent retire, or
/// freeing an already-reclaimed block). Disambiguate by arena membership —
/// arenas are never unmapped, so an our-arena address with a cleared page-map
/// entry is still ours and must NOT go to the system allocator (glibc would
/// abort with "free(): invalid pointer"). Out of line in the preload cdylib so
/// the common free path inlines into the exported entry point.
///
/// # Safety
/// `ptr` was passed to `free` and has no page-map entry.
#[cfg_attr(override_export, cold)]
// The early `return` after handing a foreign pointer to the system free is
// needed only when `secure`/`debug` is also on (otherwise control would fall
// through to the abort); in the `override`-only config it is the last statement,
// which clippy flags — but it is genuinely cfg-conditional, so allow it.
#[allow(clippy::needless_return)]
unsafe fn free_foreign_or_invalid(ptr: NonNull<u8>) {
    // `ptr` is consumed below only in the `override`+`std` configuration; tie it
    // off up front so every feature combination (e.g. `secure`/`debug` without
    // `override`, where only the abort path runs) keeps it "used". `NonNull` is
    // `Copy`, so the later reads are unaffected, and being first this is never
    // unreachable after the diverging abort.
    let _ = ptr;
    #[cfg(all(feature = "override", feature = "std"))]
    if !subproc_main().owns_address(ptr.as_ptr()) {
        // Genuinely foreign pointer: hand it back to the real system free.
        // SAFETY: not in any of our arenas ⇒ it is a system allocation safe
        // to hand to the real libc free.
        unsafe { crate::sysalloc::free(ptr.as_ptr() as *mut core::ffi::c_void) };
        return;
    }
    // Ours-but-unmapped (or, in non-override builds, any unmapped pointer): an
    // invalid/double free of one of our blocks. Default builds treat it as a
    // no-op; hardened builds abort. Never forward it to the system.
    #[cfg(any(feature = "secure", feature = "debug"))]
    report_corruption_and_abort(
        "mimalloc-rs: invalid free (pointer not owned by this allocator)\n",
    );
}

/// Free a block previously returned by [`Heap::alloc`] (heap-independent).
///
/// Finds the owning page through the page-map and returns the block to it.
/// Owner frees take the local deferred-free path; non-owner (cross-thread)
/// frees push the block onto the page's atomic `xthread_free` list.
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
    // The page's const fields (block size, area start) are immutable after init,
    // so they are sound to read from any thread.
    // SAFETY: the page-map only stores valid page headers.
    let bs = unsafe { Page::raw_block_size(page_ptr) };
    crate::stats::on_free(bs);

    // Recover the block start from a (possibly interior) pointer. A block-start
    // pointer (`off == 0`, the overwhelmingly common case) skips the divide; only
    // an interior pointer from a large-alignment `alloc_aligned` — which sets the
    // page's `has_interior` flag — needs the normalization.
    let recover_block = |p: NonNull<u8>| -> NonNull<u8> {
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
    };

    #[cfg(feature = "std")]
    {
        // Fold the owner-vs-cross-thread decision and the page flag check into a
        // single XOR (ports `mi_free_ex`, `free.c:185-205`): `xtid == 0` means we
        // own the page *and* it carries no flags (not full, no interior) — the
        // fast local path; the flag bits route the rare full/interior cases to the
        // generic paths at no extra cost on the common path. Two distinct thread
        // ids differ above the flag mask (ids have clear low bits), so
        // `xtid & !MI_PAGE_FLAG_MASK == 0` iff we are the owner.
        // SAFETY: reads the raw xthread_id atomically without forming `&Page`.
        let raw = unsafe { Page::xthread_id_raw(page_ptr) };
        let xtid = crate::init::current_tid() ^ raw;
        if xtid & !MI_PAGE_FLAG_MASK == 0 {
            // Local (we own the page). `xtid == 0` ⇒ block-start pointer;
            // otherwise an interior/full-flagged page → recover the block start.
            let block = if xtid == 0 { ptr } else { recover_block(ptr) };
            // SAFETY: this thread owns the page.
            unsafe {
                // Hardened builds: detect frees of pointers outside the block
                // area and double frees before mutating the free list.
                #[cfg(any(feature = "secure", feature = "debug"))]
                {
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
                (*page_ptr).free_local(block);
                // If the page is now fully free, retire it (return its slices).
                if (*page_ptr).is_all_free() {
                    retire_page(page_ptr);
                }
            }
        } else {
            // Cross-thread (or abandoned) page: atomic Treiber push (touches only
            // the atomic + block). `xtid & FLAG_MASK == 0` ⇒ block-start pointer;
            // a flagged page → recover the block start.
            let block = if xtid & MI_PAGE_FLAG_MASK == 0 {
                ptr
            } else {
                recover_block(ptr)
            };
            // SAFETY: live page and block.
            let claimed = unsafe { Page::thread_free_push(page_ptr, block) };
            if claimed {
                // The page was abandoned and this push transitioned it
                // unowned→owned: we now exclusively own it and must collect it,
                // then free / reabandon / unown (ports mi_free_block_mt →
                // mi_free_try_collect_mt).
                // SAFETY: we exclusively own the page now; `block` is the head we
                // just pushed (enables the no-atomic partial collect).
                unsafe {
                    free_try_collect_mt(page_ptr, block.as_ptr() as *mut crate::free_list::Block)
                };
            }
        }
    }
    #[cfg(not(feature = "std"))]
    {
        // Without std TLS we assume single-owner frees; embedders that share
        // heaps across tasks must route cross-task frees themselves.
        let block = recover_block(ptr);
        // SAFETY: single-owner assumption.
        unsafe {
            (*page_ptr).free_local(block);
            if (*page_ptr).is_all_free() {
                retire_page(page_ptr);
            }
        }
    }
}

/// Report memory corruption (invalid/double free) and abort. Hardened builds
/// only (`secure`/`debug`); mirrors mimalloc's fail-fast on detected misuse.
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
        // No portable abort without std; halt this thread so the corruption
        // cannot propagate (embedders may install their own panic/abort hook).
        loop {
            core::hint::spin_loop();
        }
    }
}

/// Retire a now-empty page: return its slices to the owning arena and clear its
/// page-map entries, so memory footprint tracks the live set rather than the
/// peak. To avoid alloc/free churn on the common single-page case, the sole
/// remaining page of a (non-huge) bin is kept for reuse.
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
    // Keep the last page of a normal bin to avoid rebuild churn; always retire
    // huge pages (each is a distinct large mapping) and surplus pages.
    if bin != MI_BIN_HUGE && heap.pages[bin].len() <= 1 {
        return;
    }
    // Clear any fast-path entries pointing at this page before its memory is
    // released, so the direct lookup can never dangle.
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
    // Drive delayed purging opportunistically as pages drain (cheap when nothing
    // is due). Mirrors v3 retiring a page → `_mi_arenas_collect` → try-purge.
    heap.subproc.try_purge(false);
}

/// Return a page's slices to its arena and drop its address→page mappings.
///
/// ## Cross-thread safety (why releasing an empty page cannot race a foreign free)
/// A block freed by a non-owner thread is pushed onto `xthread_free` and stays
/// counted in `Page::used` until the owner *collects* it; a cross-thread free
/// never decrements `used`. Therefore `used == 0` (the precondition for getting
/// here) implies every block — including any freed by other threads — has
/// already been collected. Collection swaps `xthread_free` with `Acquire`, which
/// synchronizes-with the foreign push's `Release` CAS; so every access a foreign
/// freer makes to this page (page-map lookup, header reads, the block-link write,
/// the CAS) *happens-before* the collect, hence before this release. A foreign
/// freer performs no access to the page after its push returns. Thus when the
/// slices are returned here, no other thread can still be touching the page.
/// (Double frees are caller UB and are caught separately under `secure`/`debug`.)
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

/// We just claimed a previously-abandoned page by freeing a block into it
/// (ports `mi_free_try_collect_mt`). With the page exclusively ours: collect, then
/// (1) free it if now empty, else (3) reabandon-to-mapped if it has space again,
/// else (4) release ownership. v3's step 2 — reclaim into the originating theap —
/// is deferred (see `docs/DESIGN-fe1-ownership.md` §7); a freed-into page returns
/// to the registry and is reclaimed on the next allocation instead.
///
/// `mt_free` is the block the caller just pushed onto `xthread_free` (its head),
/// letting the first collect use the no-atomic [`Page::collect_partly`] for small
/// blocks (ports `mi_free_try_collect_mt`'s `_partly` fast path); larger blocks
/// and all retries take the full atomic [`Page::collect_free`].
///
/// # Safety
/// The calling thread exclusively owns `page_ptr` (claimed via the ownership bit);
/// `mt_free` is the block it just pushed onto `page_ptr`'s `xthread_free`.
#[cfg(feature = "std")]
unsafe fn free_try_collect_mt(page_ptr: *mut Page, mt_free: *mut crate::free_list::Block) {
    // SAFETY: const field; small blocks may use the no-atomic partial collect.
    let small = unsafe { Page::raw_block_size(page_ptr) } <= MI_SMALL_SIZE_MAX;
    let mut first = true;
    loop {
        if first && small {
            // First pass: collect the rest of the thread-free list without the
            // atomic swap (we already hold `mt_free`, the current head).
            // SAFETY: owner; `mt_free` is the just-pushed head.
            unsafe { (*page_ptr).collect_partly(mt_free) };
        } else {
            // SAFETY: we own the page; drain cross-thread + local frees (used).
            unsafe { (*page_ptr).collect_free() };
        }
        first = false;

        // 1. All blocks free → unabandon (clear any registry bit) and return the
        //    slices to the arena.
        // SAFETY: owner.
        if unsafe { (*page_ptr).is_all_free() } {
            // SAFETY: owner; provenance set at creation.
            unsafe {
                unabandon_if_mapped(page_ptr);
                release_page_slices(page_ptr);
            }
            return;
        }

        // 3. Reabandon-to-mapped: a page with free space that is not yet mapped
        //    becomes findable for reclaim-on-alloc. Register it (set the bitmap
        //    bit) and stamp the mapped state *before* releasing ownership, so a
        //    concurrent reclaimer that finds the bit must lose the ownership race.
        // SAFETY: owner; provenance set at creation.
        if unsafe { !(*page_ptr).is_full() && !(*page_ptr).is_abandoned_mapped() } {
            // SAFETY: owner; the page's slices belong to this arena.
            unsafe {
                let bin = (*page_ptr).bin() as usize;
                let arena = (*page_ptr).owning_arena();
                if !arena.is_null() {
                    (*arena).page_abandon((*page_ptr).slice_index, bin);
                    (*page_ptr).set_owner(MI_THREADID_ABANDONED_MAPPED);
                }
            }
        }

        // 4. Release ownership. If a concurrent free pushed a block in the
        //    window, `try_unown` fails — loop to re-collect and re-evaluate (the
        //    page may now be freeable, or already mapped).
        // SAFETY: owner; just collected.
        if unsafe { (*page_ptr).try_unown() } {
            return;
        }
    }
}

impl Drop for Heap {
    /// On thread exit, hand off this heap's pages so their memory is not
    /// stranded: empty pages are released to the arena; pages with live blocks
    /// (still held by the application, to be freed cross-thread later) are
    /// abandoned for another thread to reclaim.
    fn drop(&mut self) {
        for b in 0..MI_BIN_COUNT {
            let mut cur = self.pages[b].first();
            while !cur.is_null() {
                // SAFETY: the bin queue holds valid pages owned by this heap.
                let next = unsafe { (*cur).next.get() };
                // SAFETY: owner thread; draining our own queue.
                unsafe {
                    self.pages[b].remove(cur);
                    (*cur).collect_free();
                    if (*cur).is_all_free() {
                        release_page_slices(cur);
                    } else if (*cur).is_full() {
                        // Full ⇒ abandoned but **unmapped** (kept out of the
                        // registry; resurrected only when a later free claims it).
                        (*cur).set_owner(MI_THREADID_ABANDONED);
                        (*cur).set_unowned();
                    } else {
                        // Has free space ⇒ abandoned **mapped**: register it (set
                        // the bitmap bit) and stamp the mapped state before
                        // releasing ownership, so a reclaimer that finds the bit
                        // must win the ownership race first.
                        self.subproc.abandon_page(cur, b);
                        (*cur).set_owner(MI_THREADID_ABANDONED_MAPPED);
                        (*cur).set_unowned();
                    }
                }
                cur = next;
            }
        }
    }
}

/// Usable bytes reachable from `ptr` within its block.
///
/// For a block-start pointer this is the full block size; for an *interior*
/// pointer (handed out by [`Heap::alloc_aligned`] for large alignments) it is
/// the block size minus the in-block offset — i.e. exactly the space the caller
/// may write. Returning the full block size here would be unsound: `realloc`
/// would believe more space is available than there is.
///
/// # Safety
/// `ptr` must be a live allocation from this allocator.
pub unsafe fn usable_size(ptr: NonNull<u8>) -> usize {
    let page_ptr = page_map::lookup(ptr.addr().get()) as *mut Page;
    if page_ptr.is_null() {
        // Null page-map lookup is ambiguous (see `free`): disambiguate by arena
        // membership. A genuinely foreign pointer reports the system usable
        // size; an our-arena pointer with a cleared page-map entry is not a
        // live block, so it has no usable size (0) — never query the system
        // allocator about a pointer it does not own.
        #[cfg(all(feature = "override", feature = "std"))]
        if !subproc_main().owns_address(ptr.as_ptr()) {
            // SAFETY: not in any of our arenas ⇒ `ptr` is a system allocation.
            return unsafe { crate::sysalloc::usable_size(ptr.as_ptr() as *mut core::ffi::c_void) };
        }
        return 0;
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
        // small bins have the expected double-word sizes
        assert_eq!(t[1], 8);
        assert_eq!(t[2], 16);
        assert_eq!(t[bin(24)], 32);
        // non-decreasing across bins that are populated
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

    // The following cross-thread tests free *reconstructed raw addresses* from
    // worker threads, exercising the retire/abandon/reclaim machinery against
    // the process-global page-map and arena. Run in one process alongside many
    // other heaps, a freed address can land in the brief window where its page
    // was retired (page-map entry cleared, slices returned to the arena) — a
    // null page-map lookup. They run under every feature combo, including
    // `override`: the foreign-vs-ours decision is made by *arena membership*
    // (`Subproc::owns_address`), not page-map presence, so an our-arena address
    // with a cleared page-map entry is correctly treated as ours (no-op /
    // hardened-build abort) and is never forwarded to the system allocator.
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

#[cfg(all(test, feature = "override", feature = "std"))]
mod override_tests {
    use super::*;

    #[test]
    fn foreign_free_is_forwarded_not_aborted() {
        // A block from the REAL system allocator is foreign to us. Freeing it
        // through our path must forward to system free (never leak/abort) — the
        // test simply completing is the proof.
        // SAFETY: standard libc usage.
        unsafe {
            let p = libc::malloc(64) as *mut u8;
            assert!(!p.is_null());
            assert!(!is_in_heap_region(p), "system block must be foreign");
            // touch the block to prove it is real, valid memory
            core::ptr::write_bytes(p, 0xAB, 64);
            assert_eq!(*p, 0xAB);
            // our free forwards foreign pointers to the real system free
            free(NonNull::new(p).unwrap());

            // One of ours behaves normally.
            let h = Heap::new(crate::init::process_keys(), crate::init::current_tid());
            let q = h.alloc(64).unwrap();
            assert!(is_in_heap_region(q.as_ptr()), "our block must be in-region");
            free(q);
        }
    }

    #[test]
    fn foreign_realloc_is_forwarded() {
        // A system block reallocated through our path must go to system realloc:
        // non-null, contents preserved. The result is a system pointer, so our
        // free forwards it back to the system allocator.
        // SAFETY: standard libc usage.
        unsafe {
            let p = libc::malloc(32) as *mut u8;
            assert!(!p.is_null());
            for i in 0..32usize {
                *p.add(i) = (i as u8).wrapping_mul(7);
            }
            let np = crate::init::realloc(NonNull::new(p).unwrap(), 128)
                .expect("system realloc must return non-null");
            let np = np.as_ptr();
            for i in 0..32usize {
                assert_eq!(*np.add(i), (i as u8).wrapping_mul(7), "pattern preserved");
            }
            assert!(
                !is_in_heap_region(np),
                "reallocated block is still a system pointer"
            );
            // free the (system) result via our forwarding path
            free(NonNull::new(np).unwrap());
        }
    }

    #[test]
    fn foreign_usable_size_is_forwarded() {
        // SAFETY: standard libc usage.
        unsafe {
            let p = libc::malloc(48) as *mut u8;
            assert!(!p.is_null());
            let sz = usable_size(NonNull::new(p).unwrap());
            assert!(sz >= 48, "system usable_size must cover the request: {sz}");
            free(NonNull::new(p).unwrap());
        }
    }
}
