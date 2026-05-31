// SPDX-License-Identifier: MIT
//! Heaps (ports the allocation core of `src/heap.c` / `src/alloc.c`).
//!
//! A heap owns one [`PageQueue`] per size-class bin and turns size requests into
//! blocks: pick the bin, find a page with a free block (or carve a new page from
//! an arena via the [`crate::subproc`]), and pop a block. Freeing is heap
//! independent — it finds the owning page through the [`crate::page_map`].
//!
//! v1 keeps the heap single-owner (per thread). The richer `mi_theap_t`/`tld`
//! split, the `pages_free_direct` fast array, deferred-free heartbeat, and
//! page retire/abandon are layered on in later milestones.

use core::cell::Cell;
use core::ptr::NonNull;

use crate::bits::{
    bin, MI_ARENA_SLICE_SIZE, MI_BIN_COUNT, MI_BIN_HUGE, MI_INTPTR_SIZE, MI_LARGE_MAX_OBJ_SIZE,
    MI_MAX_ALIGN_SIZE, MI_MEDIUM_MAX_OBJ_SIZE, MI_SMALL_MAX_OBJ_SIZE, MI_THREADID_ABANDONED,
};
use crate::layout::align_up;
use crate::page::Page;
use crate::page_map;
use crate::page_queue::PageQueue;
use crate::subproc::{subproc_main, Subproc};
use crate::sync::OnceBox;

/// Canonical block size for each bin (the largest request the bin serves).
fn bin_sizes() -> &'static [usize; MI_BIN_COUNT] {
    static SIZES: OnceBox<[usize; MI_BIN_COUNT]> = OnceBox::new();
    SIZES.get_or_init(|| {
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
    })
}

/// Block size for a (non-huge) bin.
#[inline]
fn bin_block_size(b: usize) -> usize {
    bin_sizes()[b]
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
    pub fn alloc(&self, size: usize) -> Option<NonNull<u8>> {
        let r = self.alloc_impl(size);
        if r.is_some() {
            crate::stats::on_alloc();
        }
        r
    }

    fn alloc_impl(&self, size: usize) -> Option<NonNull<u8>> {
        let size = size.max(MI_INTPTR_SIZE);
        let b = bin(size);
        if b >= MI_BIN_HUGE {
            return self.alloc_huge(size);
        }
        let bs = bin_block_size(b);
        // Find a page in the bin with a free block.
        let q = &self.pages[b];
        let mut cur = q.first();
        while !cur.is_null() {
            // SAFETY: queue holds valid page pointers owned by this heap.
            let page = unsafe { &*cur };
            if let Some(p) = page.alloc() {
                return Some(p);
            }
            cur = page.next.get();
        }
        // Before carving a fresh page, try to adopt an abandoned page of this
        // bin (left by an exited thread), reclaiming its memory.
        if let Some(page) = self.try_reclaim(b) {
            // SAFETY: just adopted; owned by this thread.
            if let Some(p) = unsafe { (*page).alloc() } {
                return Some(p);
            }
        }
        // No page had room: carve a new one.
        let page = self.new_page(b, bs, page_slices_for(bs))?;
        // SAFETY: freshly created, non-full page.
        unsafe { (*page).alloc() }
    }

    /// Adopt an abandoned page of `bin` (left by an exited thread): claim
    /// ownership, drain the cross-thread frees that accumulated while it was
    /// abandoned, re-home it into this heap, and return it.
    fn try_reclaim(&self, bin: usize) -> Option<*mut Page> {
        let page = self.subproc.reclaim_page(bin)?;
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
        let p = self.alloc(size + align - 1)?;
        let aligned = align_up(p.addr().get(), align);
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
        let (arena, idx, p) = self.subproc.alloc_slices(slices, true, tseq)?;
        // SAFETY: `p` is `slices` committed, slice-aligned slices owned by us.
        let page = unsafe { Page::init(p, idx, slices, bs, self.keys) };
        let page_ptr = page.as_ptr();
        // Stamp ownership so cross-thread frees route to `xthread_free`, and
        // record heap/arena/bin so the page can be retired when it empties.
        // SAFETY: page just created and owned by this thread.
        unsafe {
            page.as_ref().set_owner(self.tid);
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
        Some(page_ptr)
    }

    /// Encoding keys (for diagnostics/tests).
    #[inline]
    pub fn keys(&self) -> [usize; 2] {
        self.keys
    }
}

/// Free a block previously returned by [`Heap::alloc`] (heap-independent).
///
/// Finds the owning page through the page-map and returns the block to it.
/// Cross-thread frees are handled by M6; for now this is the owner path.
///
/// # Safety
/// `ptr` must be a live allocation from this allocator.
pub unsafe fn free(ptr: NonNull<u8>) {
    let page_ptr = page_map::lookup(ptr.addr().get()) as *mut Page;
    if page_ptr.is_null() {
        return;
    }
    // Normalize to the block start (supports interior pointers) using only the
    // page's immutable const fields — sound to read from any thread.
    // SAFETY: page-map only stores valid page headers.
    let (bs, pstart) = unsafe {
        (
            Page::raw_block_size(page_ptr),
            Page::raw_page_start(page_ptr),
        )
    };
    let off = ptr.addr().get() - pstart.addr();
    let block_start = pstart.wrapping_add((off / bs) * bs);
    // SAFETY: block_start is the start of a live block in this page.
    let block = unsafe { NonNull::new_unchecked(block_start) };
    crate::stats::on_free();

    #[cfg(feature = "std")]
    {
        // SAFETY: reads the owner tid atomically without forming `&Page`.
        let owner = unsafe { Page::owner_tid(page_ptr) };
        if owner == crate::init::current_tid() {
            // Owner path: deferred local free (touches owner-only `Cell`s).
            // SAFETY: this thread owns the page.
            unsafe {
                (*page_ptr).free_local(block);
                // If the page is now fully free, retire it (return its slices).
                if (*page_ptr).is_all_free() {
                    retire_page(page_ptr);
                }
            }
        } else {
            // Cross-thread: atomic Treiber push (touches only the atomic + block).
            // SAFETY: live page and block.
            unsafe { Page::thread_free_push(page_ptr, block) };
        }
    }
    #[cfg(not(feature = "std"))]
    {
        // Without std TLS we assume single-owner frees; embedders that share
        // heaps across tasks must route cross-task frees themselves.
        // SAFETY: single-owner assumption.
        unsafe {
            (*page_ptr).free_local(block);
            if (*page_ptr).is_all_free() {
                retire_page(page_ptr);
            }
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
    // SAFETY: page is linked in this bin queue; range was registered for it.
    unsafe {
        heap.pages[bin].remove(page_ptr);
        release_page_slices(page_ptr);
    }
}

/// Return a page's slices to its arena and drop its address→page mappings.
///
/// # Safety
/// `page_ptr` must be an empty page, already unlinked from any bin queue.
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
                    } else {
                        (*cur).set_owner(MI_THREADID_ABANDONED);
                        self.subproc.abandon_page(cur, b);
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
        let t = bin_sizes();
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

    extern crate alloc;
}
