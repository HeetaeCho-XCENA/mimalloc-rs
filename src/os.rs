// SPDX-License-Identifier: MIT
//! The OS memory layer (`src/os.c`): reserve/commit/decommit/reset/purge built
//! on top of the [`Prim`] primitive interface, plus provenance tracking via
//! [`MemId`].
//!
//! This layer owns the aligned-allocation strategy (over-allocate and trim on
//! platforms with partial free, like `mmap`) and the commit/zero bookkeeping
//! the arena layer relies on.

use core::ptr::NonNull;

use crate::bits::MI_ARENA_SLICE_SIZE;
use crate::layout::{align_down, align_up, is_aligned, is_power_of_two};
use crate::prim::{DefaultPrim, OsMemConfig, Prim};
use crate::sync::OnceBox;

/// Where a block of memory came from (`mi_memkind_t`, subset used so far).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemKind {
    /// Not allocated.
    None,
    /// Statically allocated; never freed.
    Static,
    /// Allocated by the metadata allocator (`arena-meta`).
    Meta,
    /// Allocated directly from the OS.
    Os,
    /// Allocated as huge OS pages (pinned).
    OsHuge,
    /// Carved from an arena.
    Arena,
}

/// Provenance of a memory region (`mi_memid_t`).
///
/// For [`MemKind::Os`], `base`/`size` record the *actual* OS mapping (which may
/// differ from the aligned pointer handed out) so it can be freed correctly.
#[derive(Clone, Copy, Debug)]
pub struct MemId {
    pub kind: MemKind,
    pub base: *mut u8,
    pub size: usize,
    /// Cannot be decommitted/reset/protected (e.g. huge pages).
    pub is_pinned: bool,
    /// Was committed at allocation time.
    pub initially_committed: bool,
    /// Was zero-initialized at allocation time.
    pub initially_zero: bool,
}

impl MemId {
    /// The empty memid.
    pub const fn none() -> Self {
        MemId {
            kind: MemKind::None,
            base: core::ptr::null_mut(),
            size: 0,
            is_pinned: false,
            initially_committed: false,
            initially_zero: false,
        }
    }

    /// True if this region is backed directly by the OS.
    pub fn is_os(&self) -> bool {
        matches!(self.kind, MemKind::Os | MemKind::OsHuge)
    }
}

/// Cached OS configuration (queried once).
fn config() -> &'static OsMemConfig {
    static CFG: OnceBox<OsMemConfig> = OnceBox::new();
    CFG.get_or_init(DefaultPrim::mem_config)
}

/// OS page size in bytes.
#[inline]
pub fn page_size() -> usize {
    config().page_size
}

/// Whether the OS supports freeing sub-ranges of a mapping (`mmap` ⇒ true).
#[inline]
pub fn has_partial_free() -> bool {
    config().has_partial_free
}

/// `_mi_os_good_alloc_size`: round a request up to a friendly OS size.
pub fn good_alloc_size(size: usize) -> usize {
    let align = if size < 512 * 1024 {
        page_size()
    } else if size < 256 * 1024 * 1024 {
        4 * 1024 * 1024
    } else {
        MI_ARENA_SLICE_SIZE
    };
    align_up(size, align.max(page_size()))
}

/// Round `[addr, addr+size)` to a conservative page-aligned sub-range that
/// stays strictly inside the original range (start up, end down). Returns
/// `None` if nothing remains.
fn page_align_conservative(addr: *mut u8, size: usize) -> Option<(*mut u8, usize)> {
    let ps = page_size();
    let start = addr.addr();
    let end = start + size;
    let astart = align_up(start, ps);
    let aend = align_down(end, ps);
    if aend <= astart {
        return None;
    }
    Some((addr.with_addr(astart), aend - astart))
}

/// Allocate `size` bytes aligned to `alignment` from the OS.
///
/// Returns the (aligned) pointer and a [`MemId`] recording the underlying OS
/// mapping. On `mmap`-style systems an over-allocate-and-trim strategy yields
/// exact alignment (ports `mi_os_prim_alloc_aligned`).
pub fn alloc_aligned(
    size: usize,
    alignment: usize,
    commit: bool,
    allow_large: bool,
) -> Option<(NonNull<u8>, MemId)> {
    if size == 0 {
        return None;
    }
    let ps = page_size();
    let alignment = alignment.max(ps);
    debug_assert!(is_power_of_two(alignment));
    let size = align_up(size, ps);
    let allow_large = allow_large && commit;

    // Direct allocation if alignment is small relative to granularity or size.
    let try_direct = alignment <= config().alloc_granularity || alignment <= size / 4;

    if try_direct {
        // SAFETY: size is page aligned and > 0; alignment is a power of two.
        if let Ok(a) = unsafe {
            DefaultPrim::alloc(core::ptr::null_mut(), size, alignment, commit, allow_large)
        } {
            if is_aligned(a.addr.addr(), alignment) {
                let memid = MemId {
                    kind: MemKind::Os,
                    base: a.addr,
                    size,
                    is_pinned: a.is_large,
                    initially_committed: commit,
                    initially_zero: a.is_zero,
                };
                // SAFETY: prim returned a non-null mapping.
                return Some((unsafe { NonNull::new_unchecked(a.addr) }, memid));
            }
            // Not aligned: release and fall through to over-allocation.
            // SAFETY: `a.addr`/`size` is the mapping we just made.
            let _ = unsafe { DefaultPrim::free(a.addr, size) };
        }
    }

    // Over-allocate by `alignment` and trim the unaligned ends.
    if size >= usize::MAX - alignment {
        return None;
    }
    let over = size + alignment;

    if has_partial_free() {
        // SAFETY: over is page aligned (size + pow2-of-page alignment).
        let a =
            unsafe { DefaultPrim::alloc(core::ptr::null_mut(), over, 1, commit, false) }.ok()?;
        let base = a.addr;
        let aligned = base.with_addr(align_up(base.addr(), alignment));
        let pre = aligned.addr() - base.addr();
        let mid = size;
        let post = over - pre - mid;
        if pre > 0 {
            // SAFETY: [base, base+pre) is the leading slice of our mapping.
            let _ = unsafe { DefaultPrim::free(base, pre) };
        }
        if post > 0 {
            let tail = aligned.with_addr(aligned.addr() + mid);
            // SAFETY: [aligned+mid, ...) is the trailing slice of our mapping.
            let _ = unsafe { DefaultPrim::free(tail, post) };
        }
        let memid = MemId {
            kind: MemKind::Os,
            base: aligned, // pre was freed, so the usable base is the aligned ptr
            size: mid,
            is_pinned: a.is_large,
            initially_committed: commit,
            initially_zero: a.is_zero,
        };
        // SAFETY: `aligned` lies within the committed/reserved mapping and is non-null.
        return Some((unsafe { NonNull::new_unchecked(aligned) }, memid));
    }

    // Platforms without partial free (e.g. Windows) reserve uncommitted then
    // commit only the aligned middle. Not reachable on Linux; revisited per-OS.
    None
}

/// Allocate `size` bytes (page-aligned) from the OS.
pub fn alloc(size: usize, commit: bool) -> Option<(NonNull<u8>, MemId)> {
    alloc_aligned(size, page_size(), commit, false)
}

/// Free a region described by `memid` (no-op for non-OS / static memids).
///
/// # Safety
/// `memid` must describe a live region produced by this module that is no
/// longer referenced.
pub unsafe fn free(memid: &MemId) {
    if memid.is_os() && !memid.base.is_null() && memid.size > 0 {
        // SAFETY: caller guarantees the region is live and unreferenced.
        let _ = unsafe { DefaultPrim::free(memid.base, memid.size) };
    }
}

/// Commit `[addr, addr+size)`. Returns `Some(is_zero)` on success (whether the
/// range is zeroed) or `None` if the OS refused to commit.
///
/// # Safety
/// `(addr, size)` must lie within a reserved region owned by the caller.
pub unsafe fn commit(addr: NonNull<u8>, size: usize) -> Option<bool> {
    let Some((p, n)) = page_align_conservative(addr.as_ptr(), size) else {
        return Some(true);
    };
    // SAFETY: conservative sub-range of the caller's reserved region.
    unsafe { DefaultPrim::commit(p, n).ok() }
}

/// Decommit `[addr, addr+size)`.
///
/// # Safety
/// `(addr, size)` must be a committed range owned by the caller.
pub unsafe fn decommit(addr: NonNull<u8>, size: usize) {
    if let Some((p, n)) = page_align_conservative(addr.as_ptr(), size) {
        // SAFETY: conservative sub-range of the caller's committed region.
        let _ = unsafe { DefaultPrim::decommit(p, n) };
    }
}

/// Reset `[addr, addr+size)` (contents discardable, stays accessible).
///
/// # Safety
/// `(addr, size)` must be a committed range owned by the caller.
pub unsafe fn reset(addr: NonNull<u8>, size: usize) {
    if let Some((p, n)) = page_align_conservative(addr.as_ptr(), size) {
        // SAFETY: conservative sub-range of the caller's committed region.
        let _ = unsafe { DefaultPrim::reset(p, n) };
    }
}

/// Purge `[addr, addr+size)`: hint the OS to drop the physical pages while the
/// reservation stays mapped. Returns whether the range now **needs recommit**
/// before reuse — `true` if it was decommitted (a later [`commit`] is required),
/// `false` if it was reset or left untouched (still committed). Ports
/// `_mi_os_purge_ex` (`src/os.c`).
///
/// Decision (mirrors v3): if purging is disabled (`purge_delay < 0`) it is a
/// no-op; otherwise if `purge_decommits` is set it decommits; else if
/// `allow_reset` (the whole range is committed) it resets; else it is a no-op.
/// On Linux both decommit and reset issue `MADV_DONTNEED`, so the RSS drop is
/// the same — the difference is the commit-accounting (`needs_recommit`).
///
/// # Safety
/// `(addr, size)` must be a committed range owned by the caller.
pub unsafe fn purge_ex(addr: NonNull<u8>, size: usize, allow_reset: bool) -> bool {
    if crate::options::purge_delay() < 0 {
        return false; // purging disabled
    }
    if crate::options::purge_decommits() {
        // SAFETY: forwarded contract.
        unsafe { decommit(addr, size) };
        true
    } else if allow_reset {
        // SAFETY: forwarded contract.
        unsafe { reset(addr, size) };
        false
    } else {
        false
    }
}

/// Purge `[addr, addr+size)` without commit tracking (assumes the range is
/// fully committed, so reset is allowed). Convenience over [`purge_ex`].
///
/// # Safety
/// `(addr, size)` must be a committed range owned by the caller.
pub unsafe fn purge(addr: NonNull<u8>, size: usize) {
    // SAFETY: forwarded contract.
    let _ = unsafe { purge_ex(addr, size, true) };
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn page_size_is_sane() {
        let ps = page_size();
        assert!(ps >= 4096 && is_power_of_two(ps));
    }

    #[test]
    fn aligned_alloc_direct_and_write() {
        // 256 KiB aligned to 64 KiB, committed.
        let (p, memid) = alloc_aligned(256 * 1024, MI_ARENA_SLICE_SIZE, true, false).unwrap();
        assert!(is_aligned(p.addr().get(), MI_ARENA_SLICE_SIZE));
        // SAFETY: committed region of 256 KiB.
        unsafe {
            core::ptr::write_bytes(p.as_ptr(), 0xAB, 256 * 1024);
            assert_eq!(*p.as_ptr(), 0xAB);
            assert_eq!(*p.as_ptr().add(256 * 1024 - 1), 0xAB);
            free(&memid);
        }
    }

    #[test]
    fn overallocate_for_large_alignment() {
        // size smaller than alignment forces the over-allocate-and-trim path.
        let (p, memid) = alloc_aligned(64 * 1024, 1024 * 1024, true, false).unwrap();
        assert!(is_aligned(p.addr().get(), 1024 * 1024));
        // SAFETY: committed region of 64 KiB.
        unsafe {
            core::ptr::write_bytes(p.as_ptr(), 0x5A, 64 * 1024);
            assert_eq!(*p.as_ptr().add(64 * 1024 - 1), 0x5A);
            free(&memid);
        }
    }

    #[test]
    fn purge_ex_decommit_reset_and_disabled() {
        use crate::options::{self, Opt};
        // Serialize with other option-mutating tests (process-global atomics).
        let _g = options::OPTION_TEST_LOCK.lock().unwrap();
        let save_dec = options::get(Opt::PurgeDecommits);
        let save_delay = options::get(Opt::PurgeDelay);

        let (p, memid) = alloc_aligned(128 * 1024, MI_ARENA_SLICE_SIZE, true, false).unwrap();
        // SAFETY: committed 128 KiB region for the duration of the test.
        unsafe {
            // (1) decommit path: purge reports needs_recommit; reuse needs commit.
            options::set(Opt::PurgeDelay, 1000);
            options::set(Opt::PurgeDecommits, 1);
            core::ptr::write_bytes(p.as_ptr(), 0x11, 128 * 1024);
            assert!(
                purge_ex(p, 128 * 1024, true),
                "decommit purge must report needs_recommit"
            );
            commit(p, 128 * 1024); // recommit before reuse
            assert_eq!(*p.as_ptr(), 0x00, "recommitted pages read as zero");

            // (2) reset path: stays committed (no recommit needed) and remains
            // accessible. NB: reset prefers MADV_FREE, which is *lazy* — contents
            // are indeterminate (NOT guaranteed zero), so we only assert access,
            // not the value. (A reset-purged slice is therefore "dirty"; a reuse
            // that needs zero must zero it — handled by the commit/dirty path.)
            options::set(Opt::PurgeDecommits, 0);
            core::ptr::write_bytes(p.as_ptr(), 0x22, 128 * 1024);
            assert!(
                !purge_ex(p, 128 * 1024, true),
                "reset purge stays committed"
            );
            core::ptr::write_bytes(p.as_ptr(), 0x44, 64);
            assert_eq!(
                *p.as_ptr(),
                0x44,
                "reset range stays usable without recommit"
            );

            // (3) disabled (purge_delay < 0): no-op, memory untouched.
            options::set(Opt::PurgeDelay, -1);
            core::ptr::write_bytes(p.as_ptr(), 0x33, 128 * 1024);
            assert!(!purge_ex(p, 128 * 1024, true), "disabled purge is a no-op");
            assert_eq!(*p.as_ptr(), 0x33, "disabled purge leaves memory intact");

            free(&memid);
        }
        options::set(Opt::PurgeDecommits, save_dec);
        options::set(Opt::PurgeDelay, save_delay);
    }

    #[test]
    fn reserve_commit_decommit_recommit() {
        // Reserve (no commit), then commit, write, decommit, recommit, write.
        let (p, memid) = alloc_aligned(128 * 1024, MI_ARENA_SLICE_SIZE, false, false).unwrap();
        assert!(!memid.initially_committed);
        // SAFETY: reserved region; commit then touch.
        unsafe {
            assert!(is_aligned(p.addr().get(), MI_ARENA_SLICE_SIZE));
            commit(p, 128 * 1024);
            core::ptr::write_bytes(p.as_ptr(), 0x11, 128 * 1024);
            assert_eq!(*p.as_ptr(), 0x11);
            decommit(p, 128 * 1024);
            // After MADV_DONTNEED the range is still mapped; touching gives zero.
            commit(p, 128 * 1024);
            assert_eq!(*p.as_ptr(), 0x00);
            free(&memid);
        }
    }
}
