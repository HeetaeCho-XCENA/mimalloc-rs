// SPDX-License-Identifier: MIT
//! The sub-process: process-global owner of arenas and the main heap root
//! (ports the single-subproc parts of `src/init.c` / `src/arena.c`).
//!
//! v1 implements only the **single main subproc**. Multi-tenant sub-interpreter
//! isolation (`mi_subproc_new`) is deliberately out of scope and tracked as
//! follow-up work. The main subproc is a `static`, which also serves as the
//! bootstrap seed that terminates the metadata/arena allocation cycle.

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::arena::Arena;
use crate::bits::MI_BIN_COUNT;
use crate::page::Page;
use crate::sync::SpinLock;

/// Maximum arenas a subproc tracks (`mi_subproc_t.arenas`).
pub const MAX_ARENAS: usize = 160;
/// Default slices reserved when growing the arena pool (256 MiB, committed on demand).
pub const DEFAULT_ARENA_SLICES: usize = 4096;

/// A per-bin stack of abandoned pages (left by exited threads), protected by a
/// lock. The intrusive link lives in `Page::abandoned_next`.
struct AbandonedBin {
    lock: SpinLock,
    head: UnsafeCell<*mut Page>,
}

// SAFETY: `head` and the pages' `abandoned_next` links are only ever touched
// while holding `lock`, which serializes all access.
unsafe impl Sync for AbandonedBin {}

impl AbandonedBin {
    const fn new() -> Self {
        AbandonedBin {
            lock: SpinLock::new(),
            head: UnsafeCell::new(core::ptr::null_mut()),
        }
    }
}

/// A sub-process: the arena registry shared by all its heaps.
pub struct Subproc {
    lock: SpinLock,
    arenas: [AtomicPtr<Arena>; MAX_ARENAS],
    arena_count: AtomicUsize,
    /// Pages abandoned by exited threads, per size-class bin, awaiting reclaim.
    abandoned: [AbandonedBin; MI_BIN_COUNT],
}

static MAIN: Subproc = Subproc {
    lock: SpinLock::new(),
    arenas: [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_ARENAS],
    arena_count: AtomicUsize::new(0),
    abandoned: [const { AbandonedBin::new() }; MI_BIN_COUNT],
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

    /// Visit registered arenas in order.
    fn arena_at(&self, i: usize) -> Option<NonNull<Arena>> {
        NonNull::new(self.arenas[i].load(Ordering::Acquire))
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

    /// Push a non-empty page onto the abandoned stack for its `bin` so another
    /// thread can reclaim it (called when the owning thread exits).
    ///
    /// # Safety
    /// `page` is a valid, no-longer-owned page; `bin` is its size-class bin.
    pub unsafe fn abandon_page(&self, page: *mut Page, bin: usize) {
        let ab = &self.abandoned[bin];
        let _g = ab.lock.lock();
        // SAFETY: `head` and the link are only touched under `lock`.
        unsafe {
            let head = *ab.head.get();
            (*page).set_abandoned_next(head);
            *ab.head.get() = page;
        }
    }

    /// Pop an abandoned page of `bin` to reclaim, or `None` if there are none.
    pub fn reclaim_page(&self, bin: usize) -> Option<*mut Page> {
        let ab = &self.abandoned[bin];
        let _g = ab.lock.lock();
        // SAFETY: under `lock`.
        unsafe {
            let head = *ab.head.get();
            if head.is_null() {
                return None;
            }
            *ab.head.get() = (*head).abandoned_next();
            (*head).set_abandoned_next(core::ptr::null_mut());
            Some(head)
        }
    }

    /// Number of abandoned pages for a bin (test/diagnostics).
    #[cfg(test)]
    pub fn abandoned_len(&self, bin: usize) -> usize {
        let ab = &self.abandoned[bin];
        let _g = ab.lock.lock();
        let mut n = 0;
        // SAFETY: under `lock`.
        unsafe {
            let mut p = *ab.head.get();
            while !p.is_null() {
                n += 1;
                p = (*p).abandoned_next();
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
