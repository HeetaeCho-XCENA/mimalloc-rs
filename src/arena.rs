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
use core::sync::atomic::{AtomicI64, Ordering};

use crate::arena_meta::{meta_free, meta_zalloc};
use crate::bitmap::{BChunk, Bitmap, CHUNK_BITS, FIELD_BITS};
use crate::bits::{MI_ARENA_SLICE_SHIFT, MI_ARENA_SLICE_SIZE, MI_BIN_COUNT};
use crate::os::{self, MemId};
use crate::prim::{DefaultPrim, Prim};

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
    /// OS region backing all the bitmap storage (free/commit/purge + the
    /// per-bin abandoned registries). This block is large metadata (~45 KiB for a
    /// full arena, past the small-block meta allocator), so like the page-map it
    /// comes straight from the OS; freed in [`Arena::destroy`].
    bitmap_memid: MemId,
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
    /// `set = scheduled for purge`. A freed slice's bit is set here (with a
    /// delay); [`Arena::maybe_purge`] later returns the still-free ones to the OS.
    purge_chunkmap: NonNull<BChunk>,
    purge_chunks: NonNull<BChunk>,
    /// Per-bin **abandoned-page registry** (ports v3's `pages_abandoned[bin]`).
    /// `set = an abandoned page of that bin starts at this slice`, findable for
    /// reclaim-on-alloc. The `MI_BIN_COUNT` bitmaps are laid out contiguously
    /// after the purge bitmap, each `[chunkmap][chunks; chunk_count]`; this points
    /// at the first one and bin `b`'s bitmap is at `+ b*(chunk_count+1)`.
    abandoned_base: NonNull<BChunk>,
    /// Earliest time (`clock_now_msecs`) at which a scheduled purge is due, or 0
    /// when nothing is pending. CAS'd 0→deadline by the first scheduler.
    purge_expire: AtomicI64,
}

// SAFETY: the only mutable shared state is the atomic bitmaps (BChunk = atomics)
// and `purge_expire` (atomic); all other fields are set once in `create`.
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

        // Bitmap storage: free + commit + purge + `MI_BIN_COUNT` abandoned-page
        // registries, each a chunkmap + `chunk_count` chunks laid out contiguously.
        // This block can reach ~45 KiB (past the small-block meta allocator's
        // 32 KiB limit), so it comes straight from the OS (zeroed ⇒ all-clear,
        // which is a valid empty bitmap). Residency stays low: untouched
        // abandoned-registry pages never fault in.
        let stride = chunk_count + 1;
        let bchunks = (3 + MI_BIN_COUNT) * stride;
        let bm_bytes = bchunks * core::mem::size_of::<BChunk>();
        let (bm_region, bitmap_memid) = match os::alloc(bm_bytes, true) {
            Some(v) => v,
            None => {
                // SAFETY: nothing else references the data region yet.
                unsafe { os::free(&memid) };
                return None;
            }
        };
        let bm_base = bm_region.as_ptr() as *mut BChunk;
        // SAFETY: bm_mem is `bchunks` zeroed BChunks laid out contiguously.
        let (
            free_chunkmap,
            free_chunks,
            commit_chunkmap,
            commit_chunks,
            purge_chunkmap,
            purge_chunks,
            abandoned_base,
        ) = unsafe {
            (
                NonNull::new_unchecked(bm_base),
                NonNull::new_unchecked(bm_base.add(1)),
                NonNull::new_unchecked(bm_base.add(stride)),
                NonNull::new_unchecked(bm_base.add(stride + 1)),
                NonNull::new_unchecked(bm_base.add(2 * stride)),
                NonNull::new_unchecked(bm_base.add(2 * stride + 1)),
                NonNull::new_unchecked(bm_base.add(3 * stride)),
            )
        };

        // Descriptor.
        let desc_mem = match meta_zalloc(core::mem::size_of::<Arena>()) {
            Some(p) => p,
            None => {
                // SAFETY: region + bitmap storage are ours and unreferenced.
                unsafe {
                    os::free(&memid);
                    os::free(&bitmap_memid);
                }
                return None;
            }
        };
        let arena = desc_mem.as_ptr() as *mut Arena;
        // SAFETY: `desc_mem` is a zeroed, suitably-sized, aligned block.
        unsafe {
            arena.write(Arena {
                memid,
                bitmap_memid,
                start,
                slice_count,
                chunk_count,
                free_chunkmap,
                free_chunks,
                commit_chunkmap,
                commit_chunks,
                purge_chunkmap,
                purge_chunks,
                abandoned_base,
                purge_expire: AtomicI64::new(0),
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

    #[inline]
    fn purge_bitmap(&self) -> Bitmap<'_> {
        // SAFETY: same layout as `free_bitmap`.
        unsafe {
            Bitmap::from_parts(
                self.purge_chunkmap.as_ref(),
                core::slice::from_raw_parts(self.purge_chunks.as_ptr(), self.chunk_count),
            )
        }
    }

    /// The abandoned-page registry bitmap for `bin` (`set = an abandoned page of
    /// `bin` starts at this slice`). Laid out at `abandoned_base + bin*stride`.
    #[inline]
    fn abandoned_bitmap(&self, bin: usize) -> Bitmap<'_> {
        debug_assert!(bin < MI_BIN_COUNT);
        let stride = self.chunk_count + 1;
        // SAFETY: the abandoned block holds `MI_BIN_COUNT` consecutive bitmaps,
        // each `[chunkmap][chunks; chunk_count]`; bin < MI_BIN_COUNT.
        unsafe {
            let chunkmap = self.abandoned_base.as_ptr().add(bin * stride);
            Bitmap::from_parts(
                &*chunkmap,
                core::slice::from_raw_parts(chunkmap.add(1), self.chunk_count),
            )
        }
    }

    /// Register `page` (starting at `slice_index`, size-class `bin`) in the
    /// abandoned registry so another thread can reclaim it on allocation (ports
    /// the `mi_bitmap_set(pages_abandoned[bin], slice_index)` of
    /// `_mi_arenas_page_abandon`). The caller must have just relinquished the page.
    #[inline]
    pub fn page_abandon(&self, slice_index: usize, bin: usize) {
        let was_clear = self.abandoned_bitmap(bin).set(slice_index);
        debug_assert!(was_clear, "page already in the abandoned registry");
    }

    /// Reclaim one abandoned page of `bin`, returning its start slice index (and
    /// removing it from the registry), or `None` if there are none. `tseq`
    /// spreads concurrent reclaimers. Ports the find-and-clear of
    /// `mi_arenas_page_try_find_abandoned` (FE1a: no ownership claim yet —
    /// reclaim is alloc-only, so the atomic clear is the single claim point).
    #[inline]
    pub fn reclaim_abandoned(&self, bin: usize, tseq: usize) -> Option<usize> {
        self.abandoned_bitmap(bin).try_find_and_clear(tseq)
    }

    /// Number of abandoned pages of `bin` registered in this arena.
    #[inline]
    pub fn abandoned_popcount(&self, bin: usize) -> usize {
        self.abandoned_bitmap(bin).popcount()
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

    /// Free `n` slices starting at `idx` (marks them free for reuse) and
    /// schedule them for a (delayed) purge so their physical pages are returned
    /// to the OS if they stay free. The slices are immediately reusable.
    pub fn free_slices(&self, idx: usize, n: usize) {
        debug_assert!(idx + n <= self.slice_count);
        self.free_bitmap().set_n(idx, n);
        self.schedule_purge(idx, n);
    }

    /// Count of currently-free slices.
    pub fn free_slice_count(&self) -> usize {
        self.free_bitmap().popcount()
    }

    /// Number of slices currently committed (test/diagnostics; the RSS proxy).
    #[cfg(test)]
    pub fn committed_slice_count(&self) -> usize {
        self.commit_bitmap().popcount()
    }

    /// Mark `[idx, idx+n)` for purge and arm the delay timer (ports
    /// `mi_arena_schedule_purge`). Pinned arenas and a disabled `purge_delay`
    /// (`< 0`) never purge; a zero delay purges immediately.
    fn schedule_purge(&self, idx: usize, n: usize) {
        let delay = crate::options::arena_purge_delay();
        if delay < 0 || self.memid.is_pinned {
            return;
        }
        self.purge_bitmap().set_n(idx, n);
        if delay == 0 {
            // Immediate purge (regardless of the expire timer).
            self.run_purge();
        } else {
            let expire = DefaultPrim::clock_now_msecs().saturating_add(delay);
            // Only the first scheduler since the last purge sets the deadline.
            let _ =
                self.purge_expire
                    .compare_exchange(0, expire, Ordering::AcqRel, Ordering::Relaxed);
        }
    }

    /// Purge any slices whose delay has elapsed, returning their pages to the OS.
    /// Cheap when nothing is pending (one atomic load, no clock syscall). With
    /// `force`, purges regardless of the timer. Returns whether anything was
    /// purged. Ports `mi_arena_try_purge`; safe to call from any owner thread.
    pub fn maybe_purge(&self, force: bool) -> bool {
        if self.memid.is_pinned {
            return false;
        }
        if force {
            // Force: reset the timer and scan unconditionally.
            self.purge_expire.store(0, Ordering::Release);
            return self.run_purge();
        }
        let expire = self.purge_expire.load(Ordering::Acquire);
        if expire == 0 {
            return false; // nothing scheduled — avoid the clock syscall
        }
        if expire > DefaultPrim::clock_now_msecs() {
            return false; // not due yet
        }
        // Due: claim this cycle by CAS-resetting the deadline to 0. Only the
        // winner scans — this serializes concurrent purgers and, because a
        // `schedule_purge` that re-armed `purge_expire` to a newer deadline makes
        // this CAS fail, it can never clobber the deadline of a slice freed
        // concurrently (that slice is then purged on the next cycle).
        if self
            .purge_expire
            .compare_exchange(expire, 0, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        self.run_purge()
    }

    /// Walk the purge bitmap and return each still-free range to the OS. Claims
    /// a range from the free bitmap before purging so a concurrent allocation
    /// can never hand out memory mid-`madvise`; reallocated ranges are skipped.
    /// The caller (`maybe_purge`, or `schedule_purge` for the immediate case)
    /// owns resetting `purge_expire`. Mirrors `mi_arena_try_purge` + `mi_arena_purge`.
    fn run_purge(&self) -> bool {
        let mut purged = false;
        let mut idx = 0;
        while idx < self.slice_count {
            if !self.purge_bitmap().is_set(idx) {
                idx += 1;
                continue;
            }
            // Extend the run, but never across a 64-bit **field** boundary: the
            // free-bitmap claim below (`clear_n`) is atomic all-or-nothing only
            // within a single field, and that single atomic claim is exactly what
            // makes the purge race-free against allocation. (v3 likewise claims
            // per bfield.) Adjacent fields are handled as separate runs.
            let field_end = (idx / FIELD_BITS + 1) * FIELD_BITS;
            let mut end = idx + 1;
            while end < self.slice_count && end < field_end && self.purge_bitmap().is_set(end) {
                end += 1;
            }
            let n = end - idx;
            // Claim the range from the free bitmap (atomic, all-or-nothing): only
            // purge if every slice is still free, so no allocation races the
            // `madvise`. If reallocated, skip — it will reschedule when freed.
            if self.free_bitmap().clear_n(idx, n) {
                let all_committed = self.commit_bitmap().is_set_n(idx, n);
                // SAFETY: the range is claimed (exclusively ours) and committed.
                let needs_recommit = unsafe {
                    os::purge_ex(self.slice_ptr(idx), n * MI_ARENA_SLICE_SIZE, all_committed)
                };
                if needs_recommit {
                    // Decommitted: reuse must re-commit (see `ensure_committed`).
                    self.commit_bitmap().clear_n(idx, n);
                }
                // Release the range back to the free pool.
                self.free_bitmap().set_n(idx, n);
                purged = true;
            }
            // Clear the purge marks for this run (claimed or not).
            self.purge_bitmap().clear_n(idx, n);
            idx = end;
        }
        purged
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
            let memid = a.memid;
            let bitmap_memid = a.bitmap_memid;
            os::free(&memid);
            os::free(&bitmap_memid);
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
    fn delayed_purge_waits_then_force_returns_pages() {
        // Drive purging via `force` and a *positive* delay only — never set the
        // global delay to 0/immediate, so concurrent tests' frees (which read
        // these process-global options) are never purged out from under them.
        use crate::options::{self, Opt};
        let _g = options::OPTION_TEST_LOCK.lock().unwrap();
        let (sd, sc) = (
            options::get(Opt::PurgeDelay),
            options::get(Opt::PurgeDecommits),
        );
        options::set(Opt::PurgeDelay, 1000); // 1s delay (not immediate)
        options::set(Opt::PurgeDecommits, 1);

        let arena = Arena::create(16, true).unwrap(); // eager: all 16 committed
                                                      // SAFETY: fresh arena, single-threaded test.
        unsafe {
            let a = arena.as_ref();
            assert_eq!(a.committed_slice_count(), 16);
            let (i, p) = a.alloc_slices(8, 0).unwrap();
            core::ptr::write_bytes(p.as_ptr(), 0xAB, 8 * MI_ARENA_SLICE_SIZE);
            a.free_slices(i, 8); // schedules a purge ~1s out

            // Not yet due: a non-forced purge is a no-op.
            assert!(!a.maybe_purge(false), "purge must wait for the deadline");
            #[cfg(any(feature = "debug", feature = "secure"))]
            assert_eq!(a.committed_slice_count(), 16, "nothing purged before due");

            // Force purges now: pages returned to the OS, slices reusable. The
            // commit *bitmap* only changes when the purge needs a recommit
            // (debug/secure decommit strips access → PROT_NONE; release
            // MADV_DONTNEED keeps it mapped). The RSS drop happens in both.
            assert!(a.maybe_purge(true), "force purges the due range");
            assert_eq!(a.free_slice_count(), 16, "purged slices are free again");
            #[cfg(any(feature = "debug", feature = "secure"))]
            assert_eq!(a.committed_slice_count(), 8, "force decommitted the range");

            // Reuse must recommit (if needed) and hand back usable, zeroed memory.
            let (_j, q) = a.alloc_slices(8, 0).unwrap();
            core::ptr::write_bytes(q.as_ptr(), 0xCD, 8 * MI_ARENA_SLICE_SIZE);
            assert_eq!(*q.as_ptr(), 0xCD, "reused slice is writable");
            assert_eq!(*q.as_ptr().add(8 * MI_ARENA_SLICE_SIZE - 1), 0xCD);
            assert_eq!(a.committed_slice_count(), 16, "reuse left all committed");

            Arena::destroy(arena);
        }
        options::set(Opt::PurgeDelay, sd);
        options::set(Opt::PurgeDecommits, sc);
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
