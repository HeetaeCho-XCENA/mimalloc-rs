// SPDX-License-Identifier: MIT
//! Pages (ports the `mi_page_t` core of `page.c` / `alloc.c` / `free.c`).
//!
//! A page owns a run of arena slices and serves fixed-size blocks from a sharded
//! free list. Its header lives inline at the start of its first slice; the
//! blocks follow at `page_start`.
//!
//! Three free lists (mimalloc's design):
//! * `free` — blocks `malloc` hands out (owner-only),
//! * `local_free` — blocks the owner freed, migrated to `free` on demand (keeps
//!   a monotonic heartbeat),
//! * `xthread_free` — blocks freed by *other* threads (atomic Treiber stack).
//!
//! The owner pops from `free`; other threads push to `xthread_free`; the owner
//! collects both `local_free` and `xthread_free` on demand. Pages carry their
//! owning heap/arena/bin so they can be retired or abandoned (thread exit).

use core::cell::Cell;
use core::ptr::NonNull;

use crate::atomic::{AtomicPtr, AtomicUsize, Ordering};
use crate::bits::{MI_ARENA_SLICE_SIZE, MI_INTPTR_SIZE, MI_MAX_ALIGN_SIZE, MI_PAGE_FLAG_MASK};
use crate::free_list::Block;
use crate::layout::align_up;

/// A mimalloc page: header for a run of slices serving fixed-size blocks.
#[repr(C)]
pub struct Page {
    /// Owner thread id | page flags (0/4 if abandoned). Cross-thread visible.
    pub xthread_id: AtomicUsize,
    /// Owner free list (malloc pops from here).
    free: Cell<*mut Block>,
    /// Owner-deferred frees, migrated into `free` on demand.
    local_free: Cell<*mut Block>,
    /// Cross-thread free list: an atomic Treiber stack of blocks freed by other
    /// threads. Stored as `AtomicPtr` so pointer provenance is preserved. The
    /// ownership-bit tagging used for *abandoned* page reclaim is follow-up work.
    pub xthread_free: AtomicPtr<Block>,
    /// Blocks currently handed out (alive + in thread_free).
    used: Cell<u32>,
    /// Blocks threaded onto the free lists so far.
    capacity: Cell<u32>,
    /// Total blocks the page can ever hold.
    reserved: u32,
    /// Block size in bytes (const).
    block_size: usize,
    /// Start of the block area (const).
    page_start: *mut u8,
    /// Free-list encoding keys (const).
    keys: [usize; 2],
    /// Bin-queue links (owned by the heap).
    pub next: Cell<*mut Page>,
    pub prev: Cell<*mut Page>,
    /// Provenance of the page within its arena.
    pub slice_index: usize,
    pub slice_count: usize,
    /// Owning heap (set after init; owner-only access). Used to retire the page
    /// to its bin queue when it empties.
    heap: Cell<*mut crate::heap::Heap>,
    /// Owning arena (set after init). Slices return here on retire.
    arena: Cell<*mut crate::arena::Arena>,
    /// Size-class bin index this page belongs to (set after init).
    bin: Cell<u32>,
    /// Intrusive link for a sub-process abandoned-page stack (manipulated only
    /// under the per-bin abandoned lock; atomic so cross-thread access is sound).
    abandoned_next: AtomicPtr<Page>,
}

impl Page {
    /// Initialize a page in place at the start of its slice run.
    ///
    /// Lays the header at `slice_ptr`, places blocks after it, and threads the
    /// whole free list. Returns a pointer to the in-place header.
    ///
    /// # Safety
    /// `slice_ptr` must point at `slice_count` committed, slice-aligned slices
    /// that are not otherwise in use. `block_size >= MI_INTPTR_SIZE`.
    pub unsafe fn init(
        slice_ptr: NonNull<u8>,
        slice_index: usize,
        slice_count: usize,
        block_size: usize,
        keys: [usize; 2],
    ) -> NonNull<Page> {
        debug_assert!(block_size >= MI_INTPTR_SIZE);
        let region = slice_count * MI_ARENA_SLICE_SIZE;
        let header = align_up(core::mem::size_of::<Page>(), MI_MAX_ALIGN_SIZE);
        // Align the block area so blocks get natural alignment up to max-align.
        let start_off = align_up(header, MI_MAX_ALIGN_SIZE);
        let area = region - start_off;
        let reserved = (area / block_size) as u32;

        let hdr = slice_ptr.as_ptr() as *mut Page;
        // SAFETY: slice_ptr is committed and large enough for the header.
        let page_start = unsafe { slice_ptr.as_ptr().add(start_off) };
        // SAFETY: writing a freshly-typed header into committed memory.
        unsafe {
            hdr.write(Page {
                xthread_id: AtomicUsize::new(0),
                free: Cell::new(core::ptr::null_mut()),
                local_free: Cell::new(core::ptr::null_mut()),
                xthread_free: AtomicPtr::new(core::ptr::null_mut()),
                used: Cell::new(0),
                capacity: Cell::new(reserved),
                reserved,
                block_size,
                page_start,
                keys,
                next: Cell::new(core::ptr::null_mut()),
                prev: Cell::new(core::ptr::null_mut()),
                slice_index,
                slice_count,
                heap: Cell::new(core::ptr::null_mut()),
                arena: Cell::new(core::ptr::null_mut()),
                bin: Cell::new(0),
                abandoned_next: AtomicPtr::new(core::ptr::null_mut()),
            });
            let page = &*hdr;
            page.build_free_list();
            NonNull::new_unchecked(hdr)
        }
    }

    /// Thread all `reserved` blocks onto the `free` list.
    ///
    /// # Safety
    /// Must run once, at init, before the page is shared.
    unsafe fn build_free_list(&self) {
        let mut head: *mut Block = core::ptr::null_mut();
        let mut i = self.reserved as usize;
        while i > 0 {
            i -= 1;
            // SAFETY: block i lies within the page area.
            let b = unsafe { self.page_start.add(i * self.block_size) } as *mut Block;
            // SAFETY: b is a valid, writable block slot.
            unsafe {
                (*b).set_next(head, self.keys);
            }
            head = b;
        }
        self.free.set(head);
    }

    /// Migrate `local_free` and the cross-thread `xthread_free` list into `free`.
    ///
    /// Cross-thread freed blocks were not counted against `used` by the freeing
    /// thread (only the owner mutates `used`), so we decrement `used` here.
    ///
    /// # Safety
    /// Owner-only; `self`'s `Cell` fields are not touched by other threads.
    unsafe fn collect(&self) {
        // 1. Drain the cross-thread free stack (atomic swap to empty).
        let mut tf = self
            .xthread_free
            .swap(core::ptr::null_mut(), Ordering::Acquire);
        while !tf.is_null() {
            // SAFETY: `tf` blocks were published by freeing threads via
            // `set_next`, so the link is decoded the same way (encoded under
            // `secure`/`debug`).
            let next = unsafe { (*tf).next(self.keys) };
            // SAFETY: `tf` is a valid block slot owned by this page.
            unsafe {
                (*tf).set_next(self.free.get(), self.keys);
            }
            self.free.set(tf);
            self.used.set(self.used.get() - 1);
            tf = next;
        }
        // 2. Splice the owner-local deferred frees.
        let mut lf = self.local_free.get();
        while !lf.is_null() {
            // SAFETY: lf is a valid free block on our local list.
            let next = unsafe { (*lf).next(self.keys) };
            // SAFETY: same.
            unsafe {
                (*lf).set_next(self.free.get(), self.keys);
            }
            self.free.set(lf);
            lf = next;
        }
        self.local_free.set(core::ptr::null_mut());
    }

    /// Stamp the owning thread id (with page flags in the low bits).
    #[inline]
    pub fn set_owner(&self, tid: usize) {
        self.xthread_id.store(tid, Ordering::Release);
    }

    /// Read the owning thread id (flags masked off) without forming a `&Page`
    /// (sound to call from a non-owner thread: only touches the atomic).
    ///
    /// # Safety
    /// `page` must point at a live page header.
    #[inline]
    pub unsafe fn owner_tid(page: *mut Page) -> usize {
        // SAFETY: project to the atomic field only (AtomicUsize: Sync).
        let xid = unsafe { &*core::ptr::addr_of!((*page).xthread_id) };
        xid.load(Ordering::Acquire) & !MI_PAGE_FLAG_MASK
    }

    /// Const block size, read via raw projection (immutable after init), so it
    /// is sound to read from a non-owner thread.
    ///
    /// # Safety
    /// `page` must point at a live page header.
    #[inline]
    pub unsafe fn raw_block_size(page: *mut Page) -> usize {
        // SAFETY: `block_size` is set once at init and never mutated.
        unsafe { core::ptr::read(core::ptr::addr_of!((*page).block_size)) }
    }

    /// Const block-area start, read via raw projection.
    ///
    /// # Safety
    /// `page` must point at a live page header.
    #[inline]
    pub unsafe fn raw_page_start(page: *mut Page) -> *mut u8 {
        // SAFETY: `page_start` is set once at init and never mutated.
        unsafe { core::ptr::read(core::ptr::addr_of!((*page).page_start)) }
    }

    /// Push `block` onto the page's cross-thread free stack (the freeing thread
    /// is *not* the owner). Touches only the atomic head and the block's own
    /// memory, so it is sound to call concurrently with the owner.
    ///
    /// # Safety
    /// `page` is a live page header; `block` is a live block of that page no
    /// longer used by the caller.
    pub unsafe fn thread_free_push(page: *mut Page, block: NonNull<u8>) {
        // SAFETY: project to the atomic head and read the const keys only.
        let head = unsafe { &*core::ptr::addr_of!((*page).xthread_free) };
        let keys = unsafe { Page::raw_keys(page) };
        let bp = block.as_ptr() as *mut Block;
        // The next link is written through `Block::set_next`, so it shares the
        // owner free list's (optionally encoded) representation — under `secure`/
        // `debug` the cross-thread links are encoded too, not stored in the clear.
        // SAFETY: the block is exclusively ours until the CAS publishes it.
        let b = unsafe { &*(bp as *const Block) };
        loop {
            let cur = head.load(Ordering::Acquire);
            // SAFETY: `block` is writable and at least pointer-sized.
            unsafe {
                b.set_next(cur, keys);
            }
            match head.compare_exchange_weak(cur, bp, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    /// Const free-list keys, read via raw projection (immutable after init).
    ///
    /// # Safety
    /// `page` must point at a live page header.
    #[inline]
    pub unsafe fn raw_keys(page: *mut Page) -> [usize; 2] {
        // SAFETY: `keys` is set once at init and never mutated.
        unsafe { core::ptr::read(core::ptr::addr_of!((*page).keys)) }
    }

    /// (hardened builds) Is `block` already on this page's owner free lists?
    /// Used by [`crate::heap::free`] to detect double frees. Owner-only;
    /// `O(|free| + |local_free|)`, acceptable under `secure`/`debug`.
    #[cfg(any(feature = "secure", feature = "debug"))]
    pub fn owner_lists_contain(&self, block: *mut Block) -> bool {
        for mut p in [self.free.get(), self.local_free.get()] {
            while !p.is_null() {
                if core::ptr::eq(p, block) {
                    return true;
                }
                // SAFETY: `p` is a valid free block on an owner list.
                p = unsafe { (*p).next(self.keys) };
            }
        }
        false
    }

    /// Allocate one block, or `None` if the page is full.
    pub fn alloc(&self) -> Option<NonNull<u8>> {
        let mut b = self.free.get();
        if b.is_null() {
            // SAFETY: owner path.
            unsafe { self.collect() };
            b = self.free.get();
            if b.is_null() {
                return None;
            }
        }
        // Debug: every block handed out must lie on the page's block grid; a
        // violation means the free list was corrupted (e.g. an interior pointer
        // was pushed onto it).
        debug_assert_eq!(
            (b.addr() - self.page_start.addr()) % self.block_size,
            0,
            "free list corrupted: off-grid block {:#x}",
            b.addr()
        );
        // SAFETY: b is a valid free block; advance the free list.
        let next = unsafe { (*b).next(self.keys) };
        self.free.set(next);
        self.used.set(self.used.get() + 1);
        // SAFETY: b is within the page area and non-null.
        Some(unsafe { NonNull::new_unchecked(b as *mut u8) })
    }

    /// Free a block back to this page (owner path).
    ///
    /// # Safety
    /// `p` must be a block previously returned by `self.alloc()`.
    pub unsafe fn free_local(&self, p: NonNull<u8>) {
        let b = p.as_ptr() as *mut Block;
        // SAFETY: b is a valid block slot in this page.
        unsafe {
            (*b).set_next(self.local_free.get(), self.keys);
        }
        self.local_free.set(b);
        // A live block always has `used > 0`; a violation here means a double
        // free or a foreign pointer (caller-contract violation).
        debug_assert!(self.used.get() > 0, "free of non-live block (double free?)");
        self.used.set(self.used.get().saturating_sub(1));
    }

    /// Record the owning heap, arena, and bin (called once after init, on the
    /// owner thread). Enables retiring the page when it empties.
    #[inline]
    pub fn set_provenance(
        &self,
        heap: *mut crate::heap::Heap,
        arena: *mut crate::arena::Arena,
        bin: u32,
    ) {
        self.heap.set(heap);
        self.arena.set(arena);
        self.bin.set(bin);
    }

    /// Owning heap pointer (owner-only).
    #[inline]
    pub fn owning_heap(&self) -> *mut crate::heap::Heap {
        self.heap.get()
    }

    /// Owning arena pointer.
    #[inline]
    pub fn owning_arena(&self) -> *mut crate::arena::Arena {
        self.arena.get()
    }

    /// Size-class bin index.
    #[inline]
    pub fn bin(&self) -> u32 {
        self.bin.get()
    }

    /// Migrate cross-thread + local frees into the `free` list (owner path).
    /// Called when adopting an abandoned page or before checking emptiness.
    ///
    /// # Safety
    /// Caller must own the page (no other thread runs the owner path).
    #[inline]
    pub unsafe fn collect_free(&self) {
        // SAFETY: forwarded owner-only contract.
        unsafe { self.collect() }
    }

    /// Abandoned-stack link accessors (manipulated under the per-bin lock).
    #[inline]
    pub fn abandoned_next(&self) -> *mut Page {
        self.abandoned_next.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_abandoned_next(&self, p: *mut Page) {
        self.abandoned_next.store(p, Ordering::Relaxed);
    }

    /// Block size served by this page.
    #[inline]
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Number of live (handed-out) blocks.
    #[inline]
    pub fn used(&self) -> u32 {
        self.used.get()
    }

    /// Total blocks the page can hold.
    #[inline]
    pub fn reserved(&self) -> u32 {
        self.reserved
    }

    /// Start of the block area.
    #[inline]
    pub fn page_start(&self) -> *mut u8 {
        self.page_start
    }

    /// Are all blocks free?
    #[inline]
    pub fn is_all_free(&self) -> bool {
        self.used.get() == 0
    }

    /// Is the page out of immediately-available blocks (after a collect)?
    pub fn is_full(&self) -> bool {
        if !self.free.get().is_null() {
            return false;
        }
        // SAFETY: owner path.
        unsafe { self.collect() };
        self.free.get().is_null()
    }

    /// Does block-aligned `ptr` belong to this page's area?
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.page_start.addr();
        let a = ptr.addr();
        a >= base && a < base + (self.reserved as usize) * self.block_size
    }

    /// Index of the block containing `ptr` (assumes [`Page::contains`]).
    pub fn block_index(&self, ptr: *const u8) -> usize {
        (ptr.addr() - self.page_start.addr()) / self.block_size
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::arena::Arena;

    fn with_page<R>(block_size: usize, f: impl FnOnce(&Page) -> R) -> R {
        // A single 64 KiB slice page.
        let arena = Arena::create(8, true).unwrap();
        // SAFETY: fresh arena.
        unsafe {
            let a = arena.as_ref();
            let (idx, p) = a.alloc_slices(1, 0).unwrap();
            let page = Page::init(p, idx, 1, block_size, [0x1234_5678, 0x9abc_def0]);
            let r = f(page.as_ref());
            a.free_slices(idx, 1);
            Arena::destroy(arena);
            r
        }
    }

    #[test]
    fn alloc_all_distinct_aligned_then_full() {
        with_page(64, |page| {
            let reserved = page.reserved() as usize;
            assert!(reserved > 100);
            let mut ptrs = alloc::vec::Vec::new();
            for _ in 0..reserved {
                let p = page.alloc().unwrap();
                assert_eq!(p.addr().get() % 16, 0, "block must be ≥16-aligned");
                assert!(page.contains(p.as_ptr()));
                ptrs.push(p.addr().get());
            }
            assert!(page.alloc().is_none(), "page should be full");
            assert_eq!(page.used() as usize, reserved);
            // all distinct and exactly block_size apart
            ptrs.sort_unstable();
            ptrs.dedup();
            assert_eq!(ptrs.len(), reserved);
            for w in ptrs.windows(2) {
                assert_eq!(w[1] - w[0], 64);
            }
        });
    }

    #[test]
    fn free_then_realloc_reuses_after_collect() {
        // mimalloc semantics: `local_free` blocks are NOT reused until `free`
        // is exhausted (a deferred-free "heartbeat"). So we drain the page,
        // free a few, and confirm the next allocations return exactly those.
        with_page(128, |page| {
            let reserved = page.reserved() as usize;
            // SAFETY: blocks come from this page.
            unsafe {
                let mut all = alloc::vec::Vec::new();
                for _ in 0..reserved {
                    all.push(page.alloc().unwrap());
                }
                assert!(page.alloc().is_none());
                // free three specific blocks
                let freed = [all[10], all[20], all[30]];
                for &p in &freed {
                    page.free_local(p);
                }
                assert_eq!(page.used() as usize, reserved - 3);
                // free was empty, so the next allocs collect local_free and must
                // return exactly the three freed blocks.
                let mut reused = alloc::vec::Vec::new();
                for _ in 0..3 {
                    reused.push(page.alloc().unwrap().addr().get());
                }
                assert_eq!(page.used() as usize, reserved);
                for &p in &freed {
                    assert!(
                        reused.contains(&p.addr().get()),
                        "freed block must be reused"
                    );
                }
                assert!(page.alloc().is_none(), "page full again");
            }
        });
    }

    #[test]
    fn write_blocks_no_overlap() {
        with_page(64, |page| {
            // SAFETY: blocks are within committed page memory.
            unsafe {
                let p0 = page.alloc().unwrap();
                let p1 = page.alloc().unwrap();
                core::ptr::write_bytes(p0.as_ptr(), 0x11, 64);
                core::ptr::write_bytes(p1.as_ptr(), 0x22, 64);
                // writing p1 must not have touched p0
                assert_eq!(*p0.as_ptr(), 0x11);
                assert_eq!(*p1.as_ptr(), 0x22);
            }
        });
    }

    /// Hardened-build double-free detection primitive: a freed block must be
    /// discoverable on the owner free lists (which is how `free` rejects a
    /// second free of the same pointer).
    #[cfg(any(feature = "secure", feature = "debug"))]
    #[test]
    fn detects_freed_block_for_double_free_guard() {
        with_page(64, |page| {
            // SAFETY: block comes from this page.
            unsafe {
                let a = page.alloc().unwrap();
                let ab = a.as_ptr() as *mut crate::free_list::Block;
                assert!(
                    !page.owner_lists_contain(ab),
                    "a live block is not on a free list"
                );
                page.free_local(a);
                assert!(
                    page.owner_lists_contain(ab),
                    "a freed block must be detectable (double-free guard)"
                );
            }
        });
    }

    extern crate alloc;
}

#[cfg(all(test, loom))]
mod loom_tests {
    //! Models the `xthread_free` Treiber stack protocol (push + swap-drain) in
    //! isolation — proving the contended CAS loop loses and duplicates nothing.
    extern crate alloc;
    use crate::atomic::{AtomicPtr, Ordering};
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::cell::UnsafeCell;

    struct Node {
        next: UnsafeCell<*mut Node>,
        id: usize,
    }
    // SAFETY: `next` is only written by the pushing thread before publish and
    // only read by the single draining thread after the swap — no concurrent
    // access to the cell itself; the head pointer carries the synchronization.
    unsafe impl Sync for Node {}
    unsafe impl Send for Node {}

    fn push(head: &AtomicPtr<Node>, node: *mut Node) {
        loop {
            let cur = head.load(Ordering::Acquire);
            // SAFETY: we exclusively own `node` until the CAS publishes it.
            unsafe {
                *(*node).next.get() = cur;
            }
            if head
                .compare_exchange_weak(cur, node, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    fn drain(head: &AtomicPtr<Node>) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = head.swap(core::ptr::null_mut(), Ordering::Acquire);
        while !cur.is_null() {
            // SAFETY: single consumer; node was published with release.
            let next = unsafe { *(*cur).next.get() };
            let id = unsafe { (*cur).id };
            out.push(id);
            cur = next;
        }
        out
    }

    #[test]
    fn contended_push_then_drain_no_loss_no_dup() {
        loom::model(|| {
            let head = Arc::new(AtomicPtr::<Node>::new(core::ptr::null_mut()));
            // two producers each push one distinct node
            let n0 = Box::into_raw(Box::new(Node {
                next: UnsafeCell::new(core::ptr::null_mut()),
                id: 1,
            }));
            let n1 = Box::into_raw(Box::new(Node {
                next: UnsafeCell::new(core::ptr::null_mut()),
                id: 2,
            }));
            let h0 = head.clone();
            let h1 = head.clone();
            let t0 = loom::thread::spawn(move || push(&h0, n0));
            let t1 = loom::thread::spawn(move || push(&h1, n1));
            t0.join().unwrap();
            t1.join().unwrap();

            let mut got = drain(&head);
            got.sort_unstable();
            assert_eq!(
                got,
                alloc::vec![1, 2],
                "lost or duplicated a cross-thread free"
            );

            // SAFETY: drained, no longer referenced.
            unsafe {
                drop(Box::from_raw(n0));
                drop(Box::from_raw(n1));
            }
        });
    }
}
