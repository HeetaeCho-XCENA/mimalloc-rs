// SPDX-License-Identifier: MIT
//! Arenas (ports `src/arena.c`): large OS regions carved into 64 KiB slices by
//! an atomic free bitmap. In v3 there are no segments — an arena hands slices
//! directly to pages.
//!
//! Each arena owns:
//! * a data region of `slice_count` slices (reserved or committed),
//! * a **free** bitmap (`set = free slice`) for allocation, and
//! * a **committed** bitmap tracking which slices are backed by physical memory
//!   (commit-on-demand).
//!
//! The arena descriptor and its bitmap storage are allocated from the metadata
//! allocator ([`crate::arena_meta`]), never the global allocator.

use core::ptr::NonNull;

use crate::arena_meta::{meta_free, meta_zalloc};
use crate::bitmap::{BChunk, Bitmap, CHUNK_BITS};
use crate::bits::{MI_ARENA_SLICE_SHIFT, MI_ARENA_SLICE_SIZE};
use crate::os::{self, MemId};

/// Number of bitmap `BChunk`s needed to cover `slice_count` slices.
#[inline]
fn chunks_for(slice_count: usize) -> usize {
    slice_count.div_ceil(CHUNK_BITS).max(1)
}

/// An arena: a slice-managed OS region.
///
/// Lives in metadata memory; shared between threads via `*mut Arena`. All
/// cross-thread state is in the atomic bitmaps; the other fields are immutable
/// after [`Arena::create`].
#[repr(C)]
pub struct Arena {
    memid: MemId,
    start: NonNull<u8>,
    slice_count: usize,
    chunk_count: usize,
    /// `set = free`. Storage: `[free_chunkmap][free_chunks; chunk_count]`.
    free_chunkmap: NonNull<BChunk>,
    free_chunks: NonNull<BChunk>,
    /// `set = committed`. Authoritative: an eager arena pre-sets every bit at
    /// creation, so on-demand commit and recommit-after-purge share one path.
    commit_chunkmap: NonNull<BChunk>,
    commit_chunks: NonNull<BChunk>,
}

// SAFETY: the only mutable shared state is the atomic bitmaps (BChunk = atomics);
// all other fields are set once in `create` and then read-only.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
    /// Reserve a new arena of `slice_count` slices. If `commit` is false the
    /// region is only reserved and committed per allocation.
    ///
    /// Returns a pointer to the descriptor (in metadata memory), or `None` on OOM.
    pub fn create(slice_count: usize, commit: bool) -> Option<NonNull<Arena>> {
        debug_assert!(slice_count > 0);
        let chunk_count = chunks_for(slice_count);
        // One chunkmap (512 bits) tracks at most CHUNK_BITS chunks ⇒ ≤ 16 GiB
        // per arena bitmap. Refuse larger requests rather than index past it.
        if chunk_count > CHUNK_BITS {
            return None;
        }

        // Data region.
        let size = slice_count * MI_ARENA_SLICE_SIZE;
        let (start, memid) = os::alloc_aligned(size, MI_ARENA_SLICE_SIZE, commit, false)?;

        // Bitmap storage: free(chunkmap + chunks) + commit(chunkmap + chunks).
        let bchunks = 2 * (chunk_count + 1);
        let bm_bytes = bchunks * core::mem::size_of::<BChunk>();
        let bm_mem = match meta_zalloc(bm_bytes) {
            Some(p) => p,
            None => {
                // SAFETY: nothing else references the region yet.
                unsafe { os::free(&memid) };
                return None;
            }
        };
        let bm_base = bm_mem.as_ptr() as *mut BChunk;
        // SAFETY: bm_mem is `bchunks` zeroed BChunks laid out contiguously.
        let (free_chunkmap, free_chunks, commit_chunkmap, commit_chunks) = unsafe {
            let free_chunkmap = bm_base;
            let free_chunks = bm_base.add(1);
            let commit_chunkmap = bm_base.add(1 + chunk_count);
            let commit_chunks = bm_base.add(2 + chunk_count);
            (
                NonNull::new_unchecked(free_chunkmap),
                NonNull::new_unchecked(free_chunks),
                NonNull::new_unchecked(commit_chunkmap),
                NonNull::new_unchecked(commit_chunks),
            )
        };

        // Descriptor.
        let desc_mem = match meta_zalloc(core::mem::size_of::<Arena>()) {
            Some(p) => p,
            None => {
                // SAFETY: region + bitmap storage are ours and unreferenced.
                unsafe {
                    os::free(&memid);
                    meta_free(bm_mem, bm_bytes);
                }
                return None;
            }
        };
        let arena = desc_mem.as_ptr() as *mut Arena;
        // SAFETY: `desc_mem` is a zeroed, suitably-sized, aligned block.
        unsafe {
            arena.write(Arena {
                memid,
                start,
                slice_count,
                chunk_count,
                free_chunkmap,
                free_chunks,
                commit_chunkmap,
                commit_chunks,
            });
            let a = &*arena;
            // Mark all real slices free.
            a.free_bitmap().unsafe_set_n(0, slice_count);
            // Eager arenas commit the whole region up front: pre-set every commit
            // bit so the (authoritative) commit bitmap reflects reality and the
            // alloc path does no per-slice commit. A lazy arena leaves the bits
            // clear and commits on demand in `ensure_committed`.
            if commit {
                a.commit_bitmap().unsafe_set_n(0, slice_count);
            }
        }
        NonNull::new(arena)
    }

    #[inline]
    fn free_bitmap(&self) -> Bitmap<'_> {
        // SAFETY: storage is `chunk_count` contiguous BChunks after the chunkmap.
        unsafe {
            Bitmap::from_parts(
                self.free_chunkmap.as_ref(),
                core::slice::from_raw_parts(self.free_chunks.as_ptr(), self.chunk_count),
            )
        }
    }

    #[inline]
    fn commit_bitmap(&self) -> Bitmap<'_> {
        // SAFETY: same layout as `free_bitmap`.
        unsafe {
            Bitmap::from_parts(
                self.commit_chunkmap.as_ref(),
                core::slice::from_raw_parts(self.commit_chunks.as_ptr(), self.chunk_count),
            )
        }
    }

    /// Slice count.
    #[inline]
    pub fn slice_count(&self) -> usize {
        self.slice_count
    }

    /// Pointer to slice `idx`.
    #[inline]
    pub fn slice_ptr(&self, idx: usize) -> NonNull<u8> {
        debug_assert!(idx < self.slice_count);
        // SAFETY: idx < slice_count, so this stays within the data region.
        unsafe { NonNull::new_unchecked(self.start.as_ptr().add(idx << MI_ARENA_SLICE_SHIFT)) }
    }

    /// True if `ptr` lies within this arena's data region; if so returns the
    /// containing slice index.
    pub fn slice_index_of(&self, ptr: *const u8) -> Option<usize> {
        let base = self.start.as_ptr().addr();
        let a = ptr.addr();
        if a < base || a >= base + self.slice_count * MI_ARENA_SLICE_SIZE {
            return None;
        }
        Some((a - base) >> MI_ARENA_SLICE_SHIFT)
    }

    /// Ensure slices `[idx, idx+n)` are committed. Returns false if the OS
    /// refused to commit (e.g. `ENOMEM` on a `MAP_NORESERVE` reservation).
    ///
    /// The commit bitmap is **authoritative**: an eager arena pre-set all bits at
    /// creation (so this is a no-op for it), and a purge that decommits clears
    /// the bits, so reuse re-commits here. This is what makes purge-then-reuse
    /// correct even under `debug`/`secure`, where decommit strips access
    /// (`PROT_NONE`) and a real recommit (`mprotect`) is required.
    fn ensure_committed(&self, idx: usize, n: usize) -> bool {
        if self.commit_bitmap().is_set_n(idx, n) {
            return true;
        }
        let ptr = self.slice_ptr(idx);
        // SAFETY: range is within the reserved region.
        if unsafe { os::commit(ptr, n * MI_ARENA_SLICE_SIZE) }.is_none() {
            return false;
        }
        self.commit_bitmap().set_n(idx, n);
        true
    }

    /// Allocate `n` contiguous slices. Returns the slice index and pointer.
    ///
    /// `tseq` spreads concurrent allocators across the bitmap.
    pub fn alloc_slices(&self, n: usize, tseq: usize) -> Option<(usize, NonNull<u8>)> {
        let idx = self.free_bitmap().try_find_and_clear_n(n, tseq)?;
        if !self.ensure_committed(idx, n) {
            // Commit failed: return the slices to the free bitmap.
            self.free_slices(idx, n);
            return None;
        }
        Some((idx, self.slice_ptr(idx)))
    }

    /// Free `n` slices starting at `idx` (marks them free for reuse).
    pub fn free_slices(&self, idx: usize, n: usize) {
        debug_assert!(idx + n <= self.slice_count);
        self.free_bitmap().set_n(idx, n);
    }

    /// Count of currently-free slices.
    pub fn free_slice_count(&self) -> usize {
        self.free_bitmap().popcount()
    }

    /// Tear the arena down, releasing the data region and metadata to the OS.
    ///
    /// # Safety
    /// `arena` must be a live descriptor from [`Arena::create`] with no
    /// outstanding slice references.
    pub unsafe fn destroy(arena: NonNull<Arena>) {
        // SAFETY: caller guarantees `arena` is live and unreferenced.
        unsafe {
            let a = arena.as_ref();
            let bchunks = 2 * (a.chunk_count + 1);
            let bm_bytes = bchunks * core::mem::size_of::<BChunk>();
            let bm_mem = a.free_chunkmap.cast::<u8>();
            let memid = a.memid;
            os::free(&memid);
            meta_free(bm_mem, bm_bytes);
            meta_free(arena.cast::<u8>(), core::mem::size_of::<Arena>());
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn create_alloc_free_no_leak() {
        // 64 slices (4 MiB), reserved then committed on demand.
        let arena = Arena::create(64, false).unwrap();
        // SAFETY: freshly created arena.
        unsafe {
            let a = arena.as_ref();
            assert_eq!(a.free_slice_count(), 64);

            // allocate 1 + 8 + 16 slices
            let (i1, p1) = a.alloc_slices(1, 0).unwrap();
            let (i8, p8) = a.alloc_slices(8, 0).unwrap();
            let (i16, _p16) = a.alloc_slices(16, 0).unwrap();
            assert_eq!(a.free_slice_count(), 64 - 1 - 8 - 16);

            // committed slices are writable
            core::ptr::write_bytes(p1.as_ptr(), 0xEE, MI_ARENA_SLICE_SIZE);
            core::ptr::write_bytes(p8.as_ptr(), 0x77, 8 * MI_ARENA_SLICE_SIZE);
            assert_eq!(*p1.as_ptr(), 0xEE);

            // slice_index_of round-trips
            assert_eq!(a.slice_index_of(p8.as_ptr()), Some(i8));
            assert_eq!(a.slice_index_of(p1.as_ptr().add(10)), Some(i1));

            // free everything; all slices return
            a.free_slices(i1, 1);
            a.free_slices(i8, 8);
            a.free_slices(i16, 16);
            assert_eq!(a.free_slice_count(), 64, "no slice leak");

            Arena::destroy(arena);
        }
    }

    #[test]
    fn recommit_after_commit_bit_clear() {
        // The property PC2 relies on: a purge that clears commit bits must force
        // a real recommit on the next allocation of that slice. Exercised here by
        // decommitting + clearing the bits directly (eager arena, all bits preset).
        let arena = Arena::create(4, true).unwrap();
        // SAFETY: fresh arena; single-threaded test.
        unsafe {
            let a = arena.as_ref();
            let (i, p) = a.alloc_slices(4, 0).unwrap();
            assert_eq!(i, 0);
            core::ptr::write_bytes(p.as_ptr(), 0xCC, 4 * MI_ARENA_SLICE_SIZE);
            a.free_slices(0, 4);

            // Simulate a decommit-purge of the region: drop the pages and clear
            // the commit bits (under debug/secure this also strips access).
            os::decommit(p, 4 * MI_ARENA_SLICE_SIZE);
            a.commit_bitmap().clear_n(0, 4);
            assert!(
                !a.commit_bitmap().is_set_n(0, 4),
                "purge cleared commit bits"
            );

            // Reuse must recommit before handing the slices back.
            let (j, q) = a.alloc_slices(4, 0).unwrap();
            assert_eq!(j, 0);
            core::ptr::write_bytes(q.as_ptr(), 0x99, 4 * MI_ARENA_SLICE_SIZE);
            assert_eq!(*q.as_ptr(), 0x99, "recommitted slice is writable");
            assert_eq!(*q.as_ptr().add(4 * MI_ARENA_SLICE_SIZE - 1), 0x99);
            assert!(
                a.commit_bitmap().is_set_n(0, 4),
                "reuse re-set the commit bits"
            );

            Arena::destroy(arena);
        }
    }

    #[test]
    fn exhaustion_returns_none() {
        let arena = Arena::create(8, true).unwrap();
        // SAFETY: fresh arena.
        unsafe {
            let a = arena.as_ref();
            let (_i, _p) = a.alloc_slices(8, 0).unwrap(); // takes all 8
            assert!(a.alloc_slices(1, 0).is_none());
            assert_eq!(a.free_slice_count(), 0);
            Arena::destroy(arena);
        }
    }
}
