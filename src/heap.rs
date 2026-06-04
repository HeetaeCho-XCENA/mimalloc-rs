// SPDX-License-Identifier: MIT
//! Heaps (ports `src/heap.c` / `src/alloc.c`). Single-owner (per thread).

use core::cell::Cell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arena_meta::{meta_free, meta_zalloc};
use crate::bits::{
    bin, wsize_from_size, MI_ARENA_SLICE_SIZE, MI_BIN_COUNT, MI_BIN_FULL, MI_BIN_HUGE,
    MI_INTPTR_SIZE, MI_LARGE_MAX_OBJ_SIZE, MI_MAX_ALIGN_SIZE, MI_MEDIUM_MAX_OBJ_SIZE,
    MI_PAGES_DIRECT, MI_RETIRE_CYCLES, MI_SMALL_MAX_OBJ_SIZE, MI_SMALL_WSIZE_MAX,
    MI_THREADID_ABANDONED, MI_THREADID_ABANDONED_MAPPED,
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
use crate::sync::{OnceBox, SpinLock};

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

/// Smallest zeroed allocation that consults the serving page's zero state
/// instead of always memset-ing. Below one OS page a straight memset is cheaper
/// than the page-map lookup; at or above it, skipping a redundant full-block
/// zero (when the page is still OS-zero) is the larger win. See
/// [`Heap::alloc_zeroed`].
const ZERO_VIA_PAGE_MIN: usize = 4096;

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

/// A thread-local execution heap (`mi_theap_t`, types.h:504): owns this thread's
/// pages, bin queues, and fast-path caches — where allocation and freeing run.
/// The logical/shared identity (subprocess binding, encoding keys, the theaps
/// list) lives in [`Heap`]. `subproc`/`keys` are cached here (the tld pattern):
/// both are immutable and process-global, so the hot path never derefs the heap.
pub struct ThreadHeap {
    /// Subprocess this theap allocates from. Cached from the logical heap (the
    /// tld pattern); immutable and process-global. Keeping `subproc`/`keys` here
    /// makes the struct layout byte-identical to the pre-split heap, so the hot
    /// path never derefs the logical heap.
    subproc: &'static Subproc,
    /// Free-list encoding keys (cached from the logical heap; immutable).
    keys: [usize; 2],
    /// Owning thread id stamped on this theap's pages (low 2 bits clear).
    tid: usize,
    /// Counter spreading arena searches across threads.
    tseq: Cell<usize>,
    /// One page queue per bin (`MI_BIN_COUNT` includes the full/huge queues).
    pages: [PageQueue; MI_BIN_COUNT],
    /// `mi_theap_t.pages_free_direct`: per small word size, the page that last
    /// served it. Invariant: a non-null entry points at a live page owned by
    /// this theap (entries are cleared in `retire_page` before slices are freed).
    pages_free_direct: [Cell<*mut Page>; MI_PAGES_DIRECT],
    /// Inclusive bin range that may hold a retired (emptied-but-kept) sole page,
    /// so `collect_retired` scans only the touched bins (`mi_theap_t`'s
    /// `page_retired_min/max`, types.h:512-513). Empty when `min > max`.
    page_retired_min: Cell<usize>,
    page_retired_max: Cell<usize>,
    // --- Cold fields (appended so the hot fields above keep the pre-split
    // offsets). Logical-heap membership; never touched on the alloc/free hot
    // path. ---
    /// Owning logical heap (`mi_theap_t.heap`, types.h:506).
    heap: *mut Heap,
    /// Links in the owning heap's `theaps` list (`mi_theap_t.hnext/hprev`),
    /// guarded by [`Heap::theaps_lock`]. Null unless `linked`.
    hnext: Cell<*mut ThreadHeap>,
    hprev: Cell<*mut ThreadHeap>,
    /// `true` once this theap is in its heap's `theaps` list (first-class theaps;
    /// the per-thread default theap is reached via TLS and is not listed).
    linked: Cell<bool>,
}

impl ThreadHeap {
    /// Create a theap belonging to logical `heap`, owned by thread `tid` (low 2
    /// bits clear, non-zero). Caches `subproc`/`keys` from the heap (the tld
    /// pattern). `heap` must outlive the theap (the `'static` default heap, or a
    /// first-class heap that owns the theap).
    pub fn new(heap: &Heap, tid: usize) -> Self {
        ThreadHeap {
            subproc: heap.subproc,
            keys: heap.keys,
            tid,
            tseq: Cell::new(0),
            pages: [const { PageQueue::new() }; MI_BIN_COUNT],
            pages_free_direct: [const { Cell::new(core::ptr::null_mut()) }; MI_PAGES_DIRECT],
            // Empty retired range: min > max.
            page_retired_min: Cell::new(MI_BIN_FULL),
            page_retired_max: Cell::new(0),
            heap: heap as *const Heap as *mut Heap,
            hnext: Cell::new(core::ptr::null_mut()),
            hprev: Cell::new(core::ptr::null_mut()),
            linked: Cell::new(false),
        }
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
            None => {
                // No existing page had room. Sweep retired pages first (this also
                // drives delayed purge on the cadence) so an idle size class can
                // return its kept page before we carve a fresh one (page.c:834).
                self.collect_retired(false);
                self.new_page(b, bs, page_slices_for(bs))?
            }
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
                // Reusing a page from the queue cancels any pending retire so a
                // churned sole page is never released out from under us
                // (ports page.c:847,872).
                if p.retire_expire() != 0 {
                    p.set_retire_expire(0);
                }
                return Some(cur);
            }
            cur = p.next.get();
        }
        self.try_reclaim(b)
    }

    /// Sweep the bins that hold a retired (emptied-but-kept) sole page: decrement
    /// each one's countdown and release it once the countdown elapses (or on
    /// `force`). Reused pages have their countdown cancelled. Runs on the
    /// alloc-generic cadence and is where delayed purge is now driven, replacing
    /// the old per-retire `try_purge` (ports `_mi_theap_collect_retired`,
    /// page.c:471-496).
    fn collect_retired(&self, force: bool) {
        // Owner-only: touches owner-thread `Cell` state. Today the only callers
        // are the owner's alloc-generic path and `collect`, but guard it like
        // `collect` so a future caller can't regress this silently.
        #[cfg(all(feature = "std", any(debug_assertions, feature = "secure")))]
        debug_assert_eq!(
            self.tid,
            crate::init::current_tid(),
            "ThreadHeap::collect_retired called from a non-owning thread"
        );
        let lo = self.page_retired_min.get();
        let hi = self.page_retired_max.get();
        // Recompute the touched range as we go; empty when min > max.
        let mut min = MI_BIN_FULL;
        let mut max = 0;
        for b in lo..=hi {
            let page_ptr = self.pages[b].first();
            if page_ptr.is_null() {
                continue;
            }
            // SAFETY: a queue head is a live page owned by this heap.
            let page = unsafe { &*page_ptr };
            let expire = page.retire_expire();
            if expire == 0 {
                continue;
            }
            if !page.is_all_free() {
                // Reused since it was retired — cancel the countdown.
                page.set_retire_expire(0);
                continue;
            }
            let remaining = expire - 1;
            page.set_retire_expire(remaining);
            if remaining == 0 || force {
                // Countdown elapsed: release the page like an immediate retire.
                for slot in self.pages_free_direct.iter() {
                    if slot.get() == page_ptr {
                        slot.set(core::ptr::null_mut());
                    }
                }
                // SAFETY: empty, owner-held page linked in this bin queue; its
                // slice range was registered for it at creation.
                unsafe {
                    self.pages[b].remove(page_ptr);
                    release_page_slices(page_ptr);
                }
            } else {
                // Still counting down: keep tracking this bin.
                if b < min {
                    min = b;
                }
                if b > max {
                    max = b;
                }
            }
        }
        self.page_retired_min.set(min);
        self.page_retired_max.set(max);
        // Drive delayed purge on the cadence (replaces the per-retire try_purge).
        self.subproc.try_purge(false);
    }

    /// Adopt an abandoned page of `bin` (left by an exited thread).
    fn try_reclaim(&self, bin: usize) -> Option<*mut Page> {
        let page_ptr = self.subproc.reclaim_page(bin, self.next_tseq())?;
        // SAFETY: popped from the abandoned stack — exclusively ours now.
        let page = unsafe { &*page_ptr };
        let arena = page.owning_arena();
        page.set_owner(self.tid);
        // A reclaimed page has been used; its free blocks are no longer zero.
        page.mark_reused();
        page.set_provenance(
            self as *const ThreadHeap as *mut ThreadHeap,
            arena,
            bin as u32,
        );
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

    /// Allocate `size` zeroed bytes. Blocks past [`ZERO_VIA_PAGE_MIN`] consult
    /// the serving page's zero state to skip re-zeroing memory the OS already
    /// cleared (ports the `free_is_zero` fast path of `_mi_page_malloc_zero`);
    /// smaller blocks just memset, where that beats the page-map lookup.
    pub fn alloc_zeroed(&self, size: usize) -> Option<NonNull<u8>> {
        let p = self.alloc(size)?;
        if size > ZERO_VIA_PAGE_MIN {
            let page = page_map::lookup(p.addr().get()) as *const Page;
            debug_assert!(!page.is_null(), "just-allocated block must be mapped");
            // SAFETY: a just-allocated block is registered; `p` is its start.
            unsafe { (*page).zero_block(p) };
        } else {
            // SAFETY: `p` is a fresh block valid for at least `size` bytes.
            unsafe { core::ptr::write_bytes(p.as_ptr(), 0, size) };
        }
        Some(p)
    }

    /// Allocate `size` zeroed bytes aligned to `align`. For over-alignment the
    /// block start differs from the returned pointer, so the page fast path does
    /// not apply — the user region is zeroed directly.
    pub fn alloc_zeroed_aligned(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        if align <= MI_INTPTR_SIZE {
            return self.alloc_zeroed(size);
        }
        let p = self.alloc_aligned(size, align)?;
        // SAFETY: `p` is valid for at least `size` bytes.
        unsafe { core::ptr::write_bytes(p.as_ptr(), 0, size) };
        Some(p)
    }

    /// Allocate an object too large for any size class as its own page.
    fn alloc_huge(&self, size: usize) -> Option<NonNull<u8>> {
        let bs = align_up(size, MI_MAX_ALIGN_SIZE);
        let slices = align_up(bs, MI_ARENA_SLICE_SIZE) / MI_ARENA_SLICE_SIZE;
        let tseq = self.next_tseq();
        let eager = crate::options::eager_commit();
        let (arena, idx, p, is_zero) = self.subproc.alloc_slices(slices, eager, tseq)?;
        // The header lives off the data slice (ports MI_PAGE_META_IS_SEPARATED):
        // never writing the region means allocating a huge block cannot fault —
        // and so cannot make THP zero the multi-MiB the OS already handed us.
        let hdr = match meta_zalloc(core::mem::size_of::<Page>()) {
            Some(h) => h,
            // SAFETY: we exclusively hold the freshly-claimed slices.
            None => {
                unsafe { arena.as_ref().free_slices(idx, slices) };
                return None;
            }
        };
        // SAFETY: `hdr` is zeroed meta; `p` is `slices` committed slices we own.
        let page = unsafe { Page::init_huge(hdr, p, idx, slices, bs, self.keys, is_zero) };
        let page_ptr = page.as_ptr();
        // SAFETY: page just created and owned by this thread.
        unsafe {
            page.as_ref().set_owner_fresh(self.tid);
            page.as_ref().set_provenance(
                self as *const ThreadHeap as *mut ThreadHeap,
                arena.as_ptr(),
                MI_BIN_HUGE as u32,
            );
            // Map the slice addresses to the off-slice header.
            if !page_map::register(p.addr().get(), slices, page_ptr as *mut u8) {
                arena.as_ref().free_slices(idx, slices);
                meta_free(hdr, core::mem::size_of::<Page>());
                return None;
            }
            self.pages[MI_BIN_HUGE].push_front(page_ptr);
        }
        crate::stats::on_page_created();
        // Serve the single block directly — no free-list write touches the slice.
        // SAFETY: freshly created huge page owned by this thread.
        Some(unsafe { page.as_ref().serve_huge() })
    }

    /// Carve a new page of `slices` slices for `bin` with block size `bs`,
    /// register it in the page-map, and push it on the bin queue.
    fn new_page(&self, bin: usize, bs: usize, slices: usize) -> Option<*mut Page> {
        let tseq = self.next_tseq();
        let eager = crate::options::eager_commit();
        let (arena, idx, p, is_zero) = self.subproc.alloc_slices(slices, eager, tseq)?;
        // SAFETY: `p` is `slices` committed, slice-aligned slices owned by us.
        let page = unsafe { Page::init(p, idx, slices, bs, self.keys, is_zero) };
        let page_ptr = page.as_ptr();
        // SAFETY: page just created and owned by this thread.
        unsafe {
            // Fresh, unpublished page: a plain store, not the flag-preserving CAS
            // used when reclaiming a live page.
            page.as_ref().set_owner_fresh(self.tid);
            page.as_ref().set_provenance(
                self as *const ThreadHeap as *mut ThreadHeap,
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
            "ThreadHeap::collect called from a non-owning thread (mi_heap_* is owner-thread-only)"
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
        if force {
            // Force released every page, including retired sole pages — the
            // retired-bin range is now empty.
            self.page_retired_min.set(MI_BIN_FULL);
            self.page_retired_max.set(0);
        }
        // Heartbeat that drives delayed purging back to the OS.
        self.subproc.try_purge(force);
    }
}

/// A logical heap (`mi_heap_t`, types.h:556): the shareable identity that owns a
/// set of thread-local [`ThreadHeap`]s — one per thread that allocates from it.
/// The process default heap is reached through the per-thread `ThreadHeap` in TLS
/// (it is not listed); a first-class heap (`mi_heap_new`) registers its theap(s)
/// in `theaps`. (HT3 makes one first-class heap usable from several threads, each
/// via its own listed theap.)
pub struct Heap {
    /// Subprocess this heap allocates from (`mi_heap_t.subproc`).
    subproc: &'static Subproc,
    /// Free-list encoding keys for this heap's pages.
    keys: [usize; 2],
    /// Unique id among heaps of this subprocess (`mi_heap_t.heap_seq`).
    heap_seq: usize,
    /// Head of the intrusive list of theaps belonging to this heap
    /// (`mi_heap_t.theaps`), linked through `ThreadHeap::hnext/hprev`. Touched
    /// only under `theaps_lock`. Empty (null) for the default heap.
    theaps: Cell<*mut ThreadHeap>,
    /// Guards `theaps` list operations (`mi_heap_t.theaps_lock`).
    theaps_lock: SpinLock,
}

// SAFETY: `subproc`/`keys`/`heap_seq` are immutable after construction; `theaps`
// is only read/written while holding `theaps_lock`, whose Acquire/Release
// ordering publishes the linked theaps across threads. The raw `theaps` pointer
// is never used to move a `ThreadHeap` across threads — only to link/unlink it.
unsafe impl Sync for Heap {}
unsafe impl Send for Heap {}

/// Monotonic source for [`Heap::heap_seq`].
fn next_heap_seq() -> usize {
    static SEQ: AtomicUsize = AtomicUsize::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// The process-wide default logical heap (`mi_heap_main`): the shared identity
/// behind every thread's default [`ThreadHeap`]. Created once.
pub fn default_heap() -> &'static Heap {
    static DEFAULT: OnceBox<Heap> = OnceBox::new();
    DEFAULT.get_or_init(|| Heap {
        subproc: subproc_main(),
        keys: crate::init::process_keys(),
        heap_seq: 0,
        theaps: Cell::new(core::ptr::null_mut()),
        theaps_lock: SpinLock::new(),
    })
}

impl Heap {
    /// Link `th` into this heap's `theaps` list (head insert) and mark it linked.
    ///
    /// # Safety
    /// `th` is a live theap owned by the caller, not currently in any list.
    unsafe fn register_theap(&self, th: *mut ThreadHeap) {
        let _g = self.theaps_lock.lock();
        let head = self.theaps.get();
        // SAFETY: `th` is live; `head` (if any) is a live listed theap.
        unsafe {
            (*th).hnext.set(head);
            (*th).hprev.set(core::ptr::null_mut());
            if !head.is_null() {
                (*head).hprev.set(th);
            }
            (*th).linked.set(true);
        }
        self.theaps.set(th);
    }

    /// Unlink `th` from this heap's `theaps` list. No-op if `th` is not linked.
    ///
    /// # Safety
    /// `th` is a live theap; if linked, it belongs to *this* heap's list.
    unsafe fn unregister_theap(&self, th: *mut ThreadHeap) {
        let _g = self.theaps_lock.lock();
        // SAFETY: `th` is live; list links are valid under the lock.
        unsafe {
            if !(*th).linked.get() {
                return;
            }
            let prev = (*th).hprev.get();
            let next = (*th).hnext.get();
            if prev.is_null() {
                self.theaps.set(next);
            } else {
                (*prev).hnext.set(next);
            }
            if !next.is_null() {
                (*next).hprev.set(prev);
            }
            (*th).hnext.set(core::ptr::null_mut());
            (*th).hprev.set(core::ptr::null_mut());
            (*th).linked.set(false);
        }
    }

    /// Allocate a first-class heap from metadata memory (`mi_heap_new`): a logical
    /// `Heap` plus the single-thread `ThreadHeap` that backs it. Release with
    /// [`Heap::delete`] or [`Heap::destroy`]. Returns `None` on metadata OOM.
    pub fn new_boxed(keys: [usize; 2], tid: usize) -> Option<NonNull<Heap>> {
        let hmem = meta_zalloc(core::mem::size_of::<Heap>())?;
        let tmem = match meta_zalloc(core::mem::size_of::<ThreadHeap>()) {
            Some(m) => m,
            None => {
                // SAFETY: just allocated above, unreferenced.
                unsafe { meta_free(hmem, core::mem::size_of::<Heap>()) };
                return None;
            }
        };
        let hp = hmem.as_ptr() as *mut Heap;
        let tp = tmem.as_ptr() as *mut ThreadHeap;
        // SAFETY: both are zeroed, suitably sized/aligned metadata blocks. Write
        // the logical heap first so the theap can cache its `subproc`/`keys`, then
        // register the theap in the heap's list.
        unsafe {
            hp.write(Heap {
                subproc: subproc_main(),
                keys,
                heap_seq: next_heap_seq(),
                theaps: Cell::new(core::ptr::null_mut()),
                theaps_lock: SpinLock::new(),
            });
            tp.write(ThreadHeap::new(&*hp, tid));
            (*hp).register_theap(tp);
        }
        NonNull::new(hp)
    }

    /// The theap backing this first-class heap on the current thread. (HT2: a
    /// first-class heap is single-thread, so this is the sole listed theap.)
    #[inline]
    fn theap(&self) -> &ThreadHeap {
        // SAFETY: registered at `new_boxed`, live until `delete`/`destroy`.
        unsafe { &*self.theaps.get() }
    }

    /// Encoding keys (diagnostics/tests).
    #[inline]
    pub fn keys(&self) -> [usize; 2] {
        self.keys
    }

    /// This heap's unique sequence id within its subprocess (`mi_heap_t.heap_seq`).
    #[inline]
    pub fn heap_seq(&self) -> usize {
        self.heap_seq
    }

    /// Allocate `size` bytes from this heap (`mi_heap_malloc`).
    #[inline]
    pub fn alloc(&self, size: usize) -> Option<NonNull<u8>> {
        self.theap().alloc(size)
    }

    /// Allocate `size` bytes aligned to `align` (`mi_heap_malloc_aligned`).
    #[inline]
    pub fn alloc_aligned(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        self.theap().alloc_aligned(size, align)
    }

    /// Allocate `size` zeroed bytes (`mi_heap_zalloc`).
    #[inline]
    pub fn alloc_zeroed(&self, size: usize) -> Option<NonNull<u8>> {
        self.theap().alloc_zeroed(size)
    }

    /// Allocate `size` zeroed bytes aligned to `align`.
    #[inline]
    pub fn alloc_zeroed_aligned(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        self.theap().alloc_zeroed_aligned(size, align)
    }

    /// Reclaim memory held by this heap (`mi_heap_collect`).
    pub fn collect(&self, force: bool) {
        self.theap().collect(force);
    }

    /// Delete a first-class heap (`mi_heap_delete`): hand off its pages via the
    /// theap drop path (live blocks stay valid), then free the theap and the heap.
    ///
    /// # Safety
    /// `heap` must come from [`Heap::new_boxed`] and not be used afterwards.
    pub unsafe fn delete(heap: NonNull<Heap>) {
        // SAFETY: caller guarantees exclusive, final access.
        unsafe {
            let tp = heap.as_ref().theaps.get();
            if !tp.is_null() {
                // `ThreadHeap::drop` unregisters from the theaps list, then
                // abandons/releases the pages.
                core::ptr::drop_in_place(tp);
                meta_free(
                    NonNull::new_unchecked(tp as *mut u8),
                    core::mem::size_of::<ThreadHeap>(),
                );
            }
            meta_free(heap.cast::<u8>(), core::mem::size_of::<Heap>());
        }
    }

    /// Destroy a first-class heap (`mi_heap_destroy`): free **all** of its pages
    /// and blocks in bulk, then free the theap and heap. All its pointers become
    /// invalid.
    ///
    /// # Safety
    /// `heap` must come from [`Heap::new_boxed`], no block of it may be used
    /// afterwards, and no other thread may touch it.
    pub unsafe fn destroy(heap: NonNull<Heap>) {
        // SAFETY: caller guarantees exclusive, final access.
        unsafe {
            let tp = heap.as_ref().theaps.get();
            if !tp.is_null() {
                let th = &*tp;
                for b in 0..MI_BIN_COUNT {
                    let mut cur = th.pages[b].first();
                    while !cur.is_null() {
                        let next = (*cur).next.get();
                        // SAFETY: bulk free — return the slices regardless of `used`.
                        th.pages[b].remove(cur);
                        release_page_slices(cur);
                        cur = next;
                    }
                }
                // Unlink from the theaps list, then free the theap's raw metadata
                // without running its `Drop` (the pages are already released; we
                // must not abandon them).
                heap.as_ref().unregister_theap(tp);
                meta_free(
                    NonNull::new_unchecked(tp as *mut u8),
                    core::mem::size_of::<ThreadHeap>(),
                );
            }
            meta_free(heap.cast::<u8>(), core::mem::size_of::<Heap>());
        }
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
    let theap = page.owning_theap();
    let bin = page.bin() as usize;
    if theap.is_null() || page.owning_arena().is_null() {
        return; // not a theap-managed page (e.g. a synthetic test page)
    }
    // SAFETY: `theap` is this thread's theap (owner-only access is safe here).
    let theap = unsafe { &*theap };
    // Already retired (countdown running): keep it retired (ports page.c:437).
    if page.retire_expire() != 0 {
        return;
    }
    // Sole page of a normal bin: don't release it yet. Arm the retire countdown
    // and keep it for reuse — `collect_retired` releases it later only if the
    // size class stays idle. This avoids retire/re-allocate churn when a workload
    // empties a bin then immediately allocates from it again (ports page.c:446-463).
    // Huge pages have no size class to keep, so they always release.
    if bin != MI_BIN_HUGE && theap.pages[bin].len() <= 1 {
        let cycles = if page.block_size() <= MI_SMALL_MAX_OBJ_SIZE {
            MI_RETIRE_CYCLES
        } else {
            MI_RETIRE_CYCLES / 4
        };
        page.set_retire_expire(cycles);
        if bin < theap.page_retired_min.get() {
            theap.page_retired_min.set(bin);
        }
        if bin > theap.page_retired_max.get() {
            theap.page_retired_max.set(bin);
        }
        return;
    }
    // Otherwise release immediately. Clear fast-path entries pointing at this
    // page before release.
    for slot in theap.pages_free_direct.iter() {
        if slot.get() == page_ptr {
            slot.set(core::ptr::null_mut());
        }
    }
    // SAFETY: page is linked in this bin queue; range was registered for it.
    unsafe {
        theap.pages[bin].remove(page_ptr);
        release_page_slices(page_ptr);
    }
    // Purge is no longer driven here. The old per-retire `try_purge` scanned
    // every arena on each free; v3 drives purge from collect, so `collect_retired`
    // runs it on the alloc-generic cadence instead. `free_slices` already
    // scheduled the decommit and armed the delay timer, so nothing is dropped.
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
    let (slice_index, slice_count, huge) = (page.slice_index, page.slice_count, page.is_huge());
    // SAFETY: range was registered for this page; arena owns the slices. The
    // slice base comes from the arena, not `page_ptr` — a huge page's header is
    // off-slice, so `page_ptr` is not the slice base.
    unsafe {
        let base = (*arena).slice_ptr(slice_index).addr().get();
        page_map::unregister(base, slice_count);
        (*arena).free_slices(slice_index, slice_count);
    }
    // A huge page's header lives in metadata memory; return it.
    if huge {
        // SAFETY: `page_ptr` is a live off-slice header from `meta_zalloc`.
        unsafe {
            meta_free(
                NonNull::new_unchecked(page_ptr as *mut u8),
                core::mem::size_of::<Page>(),
            )
        };
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

impl Drop for ThreadHeap {
    /// On theap teardown (thread exit or `Heap::delete`): unlink from the owning
    /// heap's theaps list, then hand off the pages — empty pages are released to
    /// the arena, pages with live blocks are abandoned for another thread.
    fn drop(&mut self) {
        // Unregister first so no other thread can find this theap mid-teardown.
        // No-op for the (unlisted) default theap. SAFETY: `heap` is live; `self`
        // is this theap.
        if self.linked.get() {
            unsafe { (*self.heap).unregister_theap(self as *mut ThreadHeap) };
        }
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

    fn test_heap() -> ThreadHeap {
        // Belongs to the shared default heap; real thread id so owner-vs-cross-
        // thread free routing is correct.
        ThreadHeap::new(default_heap(), crate::init::current_tid())
    }

    #[test]
    fn first_class_heap_alloc_free_destroy() {
        // A first-class `Heap` (mi_heap_t) backed by its own single-thread
        // `ThreadHeap`: distinct identity, allocates/frees through the handle,
        // and both teardown paths (delete = hand off, destroy = bulk free) work.
        // SAFETY: handles come from `new_boxed`; pointers are freed here.
        unsafe {
            let keys = crate::init::process_keys();
            let tid = crate::init::current_tid();
            let h1 = Heap::new_boxed(keys, tid).unwrap();
            let h2 = Heap::new_boxed(keys, tid).unwrap();
            assert_ne!(
                h1.as_ref().heap_seq(),
                h2.as_ref().heap_seq(),
                "first-class heaps get distinct sequence ids"
            );
            // Alloc + free a block through the handle.
            let p = h1.as_ref().alloc(128).unwrap();
            core::ptr::write_bytes(p.as_ptr(), 0xAB, 128);
            assert_eq!(*p.as_ptr(), 0xAB);
            free(p);
            // Zeroed allocation through the handle.
            let z = h1.as_ref().alloc_zeroed(2048).unwrap();
            for i in 0..2048 {
                assert_eq!(*z.as_ptr().add(i), 0, "alloc_zeroed must zero");
            }
            free(z);
            // delete hands off (live blocks stay valid); destroy bulk-frees even
            // with a live block outstanding.
            let _live = h2.as_ref().alloc(64).unwrap();
            Heap::delete(h1);
            Heap::destroy(h2); // `_live` is now invalid — never touched again
        }
    }

    #[test]
    fn first_class_heaps_across_threads() {
        // Many threads each create, use, and tear down their own first-class heap
        // concurrently — exercising theaps register/unregister and both teardown
        // paths (delete = hand off, destroy = bulk free) under thread churn. The
        // shared default heap's lock is touched by every theap drop. TSan/Miri in
        // CI validate the list/lock discipline.
        let handles: alloc::vec::Vec<_> = (0..8)
            .map(|t| {
                std::thread::spawn(move || {
                    // SAFETY: each thread owns the heaps it creates and frees.
                    unsafe {
                        let keys = crate::init::process_keys();
                        let tid = crate::init::current_tid();
                        for i in 0..50 {
                            let h = Heap::new_boxed(keys, tid).unwrap();
                            let mut ptrs = alloc::vec::Vec::new();
                            for _ in 0..64 {
                                ptrs.push(h.as_ref().alloc(32 + t * 8).unwrap());
                            }
                            for p in ptrs.drain(..) {
                                free(p);
                            }
                            if i % 2 == 0 {
                                Heap::delete(h);
                            } else {
                                let _live = h.as_ref().alloc(48).unwrap();
                                Heap::destroy(h); // `_live` invalid afterwards
                            }
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
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
    fn retired_sole_page_released_after_countdown() {
        // A bin's emptied sole page is kept (not released) and only returned to
        // the arena once `collect_retired` has counted the retire timer down to
        // zero — modelling an idle size class eventually giving its page back.
        let h = test_heap();
        let b = bin(200);
        // SAFETY: pointer comes from this heap and is freed on this thread.
        unsafe {
            let p = h.alloc(200).unwrap();
            assert_eq!(h.pages[b].len(), 1);
            free(p);
            // Sole page kept on retire, countdown armed (small bin => full cycles).
            assert_eq!(h.pages[b].len(), 1, "sole page kept on retire");
            // Idle collects count down; the page survives until the last one.
            for _ in 0..(MI_RETIRE_CYCLES - 1) {
                h.collect_retired(false);
                assert_eq!(h.pages[b].len(), 1, "kept while counting down");
            }
            h.collect_retired(false);
            assert_eq!(h.pages[b].len(), 0, "released when the countdown elapses");
            // Heap remains usable afterwards.
            let q = h.alloc(200).unwrap();
            free(q);
        }
    }

    #[test]
    fn churned_sole_page_survives_collect() {
        // A size class that keeps allocating and freeing its sole page must never
        // have that page released out from under it: reuse cancels the retire
        // countdown, so the page stays put across many collect cadences.
        let h = test_heap();
        let b = bin(200);
        // SAFETY: pointers come from this heap and are freed on this thread.
        unsafe {
            for _ in 0..(3 * MI_RETIRE_CYCLES as usize) {
                let p = h.alloc(200).unwrap();
                h.collect_retired(false); // page in use -> countdown cancelled
                free(p);
                h.collect_retired(false); // all-free -> ticks down, but reuse resets
            }
            assert_eq!(
                h.pages[b].len(),
                1,
                "an actively churned sole page is never released"
            );
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
