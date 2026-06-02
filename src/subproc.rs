// SPDX-License-Identifier: MIT
//! The sub-process: process-global owner of arenas and the main heap root
//! (ports the single-subproc parts of `src/init.c` / `src/arena.c`).
//!
//! v1 implements only the **single main subproc**. Multi-tenant sub-interpreter
//! isolation (`mi_subproc_new`) is deliberately out of scope and tracked as
//! follow-up work. The main subproc is a `static`, which also serves as the
//! bootstrap seed that terminates the metadata/arena allocation cycle.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::arena::Arena;
use crate::page::Page;
use crate::sync::SpinLock;

/// Maximum arenas a subproc tracks (`mi_subproc_t.arenas`).
pub const MAX_ARENAS: usize = 160;
/// Default slices reserved when growing the arena pool (256 MiB, committed on demand).
pub const DEFAULT_ARENA_SLICES: usize = 4096;

/// A sub-process: the arena registry shared by all its heaps.
///
/// Abandoned pages (left by exited threads, or evicted-full in FE2) live in the
/// per-arena `pages_abandoned[bin]` bitmap registries ([`Arena::page_abandon`] /
/// [`Arena::reclaim_abandoned`]), not in the subproc — so an abandoned page can
/// be found and reclaimed by slice index without a lock-protected stack.
pub struct Subproc {
    lock: SpinLock,
    arenas: [AtomicPtr<Arena>; MAX_ARENAS],
    arena_count: AtomicUsize,
}

static MAIN: Subproc = Subproc {
    lock: SpinLock::new(),
    arenas: [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_ARENAS],
    arena_count: AtomicUsize::new(0),
};

/// The process-global main sub-process.
pub fn subproc_main() -> &'static Subproc {
    &MAIN
}

impl Subproc {
    /// Register an arena; returns false if the registry is full.
    fn add_arena(&self, arena: NonNull<Arena>) -> bool {
        let _g = self.lock.lock();
        let idx = self.arena_count.load(Ordering::Relaxed);
        if idx >= MAX_ARENAS {
            return false;
        }
        self.arenas[idx].store(arena.as_ptr(), Ordering::Release);
        self.arena_count.store(idx + 1, Ordering::Release);
        true
    }

    /// Number of registered arenas.
    pub fn arena_count(&self) -> usize {
        self.arena_count.load(Ordering::Acquire)
    }

    /// Whether `ptr` lies within any registered arena's reserved data region.
    ///
    /// This is an **address-range** test over the arenas this subproc owns. It
    /// is independent of whether the specific slice is currently mapped to a
    /// page in the page-map: a retired/recycled page returns false from
    /// [`crate::page_map::lookup`] but its address is still inside the arena
    /// (arenas are never unmapped back to the OS — [`Arena::free_slices`] only
    /// flips bitmap bits). This is the semantically-correct basis for
    /// `mi_is_in_heap_region` and for deciding "foreign vs ours" in the
    /// `override` fallback, where a null page-map lookup alone is ambiguous
    /// (it covers both genuinely foreign pointers and our own already-retired
    /// or double-freed blocks).
    pub fn owns_address(&self, ptr: *const u8) -> bool {
        let count = self.arena_count();
        for i in 0..count {
            if let Some(arena) = self.arena_at(i) {
                // SAFETY: registered arenas stay live for the process.
                let a = unsafe { arena.as_ref() };
                if a.slice_index_of(ptr).is_some() {
                    return true;
                }
            }
        }
        false
    }

    /// Visit registered arenas in order.
    fn arena_at(&self, i: usize) -> Option<NonNull<Arena>> {
        NonNull::new(self.arenas[i].load(Ordering::Acquire))
    }

    /// Drive delayed purging across every registered arena, returning due,
    /// still-free slices to the OS. Cheap when nothing is pending (one atomic
    /// load per arena, no syscall). `force` ignores the delay timer. Called from
    /// page retire and `collect` — there is no background purge thread (matching
    /// v3, which drives purge from allocation/free/collect operations).
    pub fn try_purge(&self, force: bool) {
        let count = self.arena_count();
        for i in 0..count {
            if let Some(arena) = self.arena_at(i) {
                // SAFETY: registered arenas stay live for the whole process.
                unsafe { arena.as_ref().maybe_purge(force) };
            }
        }
    }

    /// Reserve and register a fresh arena of at least `min_slices`.
    fn reserve_arena(&self, min_slices: usize, commit: bool) -> Option<NonNull<Arena>> {
        let slices = min_slices.max(DEFAULT_ARENA_SLICES);
        let arena = Arena::create(slices, commit)?;
        if self.add_arena(arena) {
            Some(arena)
        } else {
            // Registry full: give the region back rather than leak it.
            // SAFETY: just created, unreferenced.
            unsafe { Arena::destroy(arena) };
            None
        }
    }

    /// Allocate `n` contiguous slices from any arena, growing the pool if needed.
    ///
    /// Returns the owning arena, the slice index within it, and the slice pointer.
    pub fn alloc_slices(
        &self,
        n: usize,
        commit: bool,
        tseq: usize,
    ) -> Option<(NonNull<Arena>, usize, NonNull<u8>)> {
        // Try existing arenas.
        let count = self.arena_count();
        for i in 0..count {
            if let Some(arena) = self.arena_at(i) {
                // SAFETY: registered arenas stay live for the process.
                let a = unsafe { arena.as_ref() };
                if let Some((idx, p)) = a.alloc_slices(n, tseq) {
                    return Some((arena, idx, p));
                }
            }
        }
        // None had room: reserve a new arena large enough for `n`.
        let arena = self.reserve_arena(n, commit)?;
        // SAFETY: freshly registered arena.
        let a = unsafe { arena.as_ref() };
        let (idx, p) = a.alloc_slices(n, tseq)?;
        Some((arena, idx, p))
    }

    /// Register a non-empty page in its arena's abandoned registry for `bin` so
    /// another thread can reclaim it (called when the owning thread exits, and in
    /// FE2 when a full page is evicted).
    ///
    /// # Safety
    /// `page` is a valid, no-longer-owned arena page; `bin` is its size-class bin.
    pub unsafe fn abandon_page(&self, page: *mut Page, bin: usize) {
        // SAFETY: the page carries its owning arena and start slice (set at
        // creation); both are immutable for the page's lifetime.
        unsafe {
            let arena = (*page).owning_arena();
            debug_assert!(!arena.is_null(), "abandoning a non-arena page");
            (*arena).page_abandon((*page).slice_index, bin);
        }
    }

    /// Reclaim one abandoned page of `bin` from any arena, or `None` if there are
    /// none. `tseq` spreads concurrent reclaimers across the registry.
    pub fn reclaim_page(&self, bin: usize, tseq: usize) -> Option<*mut Page> {
        let count = self.arena_count();
        for i in 0..count {
            if let Some(arena) = self.arena_at(i) {
                // SAFETY: registered arenas stay live for the process.
                let a = unsafe { arena.as_ref() };
                if let Some(idx) = a.reclaim_abandoned(bin, tseq) {
                    // The page header lives at the start of its first slice.
                    return Some(a.slice_ptr(idx).as_ptr() as *mut Page);
                }
            }
        }
        None
    }

    /// Number of abandoned pages for a bin across all arenas (test/diagnostics).
    #[cfg(test)]
    pub fn abandoned_len(&self, bin: usize) -> usize {
        let count = self.arena_count();
        let mut n = 0;
        for i in 0..count {
            if let Some(arena) = self.arena_at(i) {
                // SAFETY: registered arenas stay live for the process.
                n += unsafe { arena.as_ref() }.abandoned_popcount(bin);
            }
        }
        n
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::bits::MI_ARENA_SLICE_SIZE;
    use crate::page_map;

    #[test]
    fn alloc_from_subproc_and_page_map_roundtrip() {
        let sp = subproc_main();
        // Allocate an 8-slice run from the (possibly freshly reserved) pool.
        let (arena, idx, p) = sp.alloc_slices(8, false, 0).unwrap();
        // SAFETY: live arena.
        let a = unsafe { arena.as_ref() };

        // Register the run in the page-map as if it were one page.
        let page_hdr = p.as_ptr();
        // SAFETY: p is slice-aligned and live.
        unsafe {
            assert!(page_map::register(p.addr().get(), 8, page_hdr));
            // any address in the run resolves back to the page header
            assert_eq!(page_map::lookup(p.addr().get()), page_hdr);
            assert_eq!(
                page_map::lookup(p.addr().get() + 5 * MI_ARENA_SLICE_SIZE + 99),
                page_hdr
            );
            // the slices are committed and writable
            core::ptr::write_bytes(p.as_ptr(), 0xA5, 8 * MI_ARENA_SLICE_SIZE);

            page_map::unregister(p.addr().get(), 8);
            assert!(page_map::lookup(p.addr().get()).is_null());
            a.free_slices(idx, 8);
        }
        // (Arena stays registered in the global subproc for the process; its
        // slices are returned to the free bitmap, so there is no slice leak.)
    }
}
