// SPDX-License-Identifier: MIT
//! Address → page reverse map (ports `src/page-map.c`, the 2-level variant).
//!
//! Every 64 KiB arena slice maps to the [`crate::page::Page`] that owns it, so
//! `free(p)` can recover the page in O(1). On x64 (`MI_MAX_VABITS == 47`) the
//! map is **two-level**: a fixed top table of `2^18` entries, each lazily
//! pointing at a 64 KiB submap of `2^13` slice entries.
//!
//! Stored page pointers are kept as raw `*mut u8`; the [`crate::page`] layer
//! casts to its concrete type. Entries are published with `Release` and read
//! with `Acquire`, so a thread that observes a page pointer also observes the
//! page's initialized (const) fields.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::bits::{MI_ARENA_SLICE_SHIFT, MI_MAX_VABITS};
use crate::os;
use crate::sync::SpinLock;

/// Bits of slice index handled within one submap.
const SUB_SHIFT: usize = 13;
/// Entries per submap (8192).
const SUB_COUNT: usize = 1 << SUB_SHIFT;
/// Bits of slice index handled by the top table.
const TOP_SHIFT: usize = MI_MAX_VABITS - SUB_SHIFT - MI_ARENA_SLICE_SHIFT;
/// Top table entry count (2^18 on x64).
const TOP_COUNT: usize = 1 << TOP_SHIFT;

/// A submap: one page pointer per slice.
type Submap = [AtomicPtr<u8>; SUB_COUNT];
/// A top-table entry: pointer to a (lazily allocated) submap.
type TopEntry = AtomicPtr<Submap>;

/// Pointer to the top table (`*mut TopEntry`), lazily allocated from the OS.
static TOP: AtomicPtr<TopEntry> = AtomicPtr::new(core::ptr::null_mut());
/// Serializes top-table and submap allocation.
static INIT_LOCK: SpinLock = SpinLock::new();

/// A single shared, read-only, all-null submap. Every *unregistered* top-table
/// entry points here instead of being null, so [`lookup`] never has to branch
/// on a null submap on the hot path: reading any slot of it yields a null page
/// (the address is unmapped / not ours). This generalizes mimalloc's committed
/// entry-0 `sub0` NULL-resolution trick (`page-map.c:273-288`, where the C 2-level
/// map keeps one zeroed submap so `_mi_ptr_page(NULL) == NULL`) to the *whole*
/// table — turning C's `if (sub==NULL) return NULL` guard into "always read a
/// valid submap", which is what makes the unchecked fast path safe by
/// construction rather than by relying on the caller's pointer being mapped.
static ZERO_SUBMAP: Submap = [const { AtomicPtr::new(core::ptr::null_mut()) }; SUB_COUNT];

/// Address of the shared zero-submap as a `*mut Submap`.
#[inline]
fn zero_submap() -> *mut Submap {
    core::ptr::addr_of!(ZERO_SUBMAP) as *mut Submap
}

#[inline]
fn slice_index(addr: usize) -> usize {
    addr >> MI_ARENA_SLICE_SHIFT
}

#[inline]
fn split(u: usize) -> (usize, usize) {
    (u >> SUB_SHIFT, u & (SUB_COUNT - 1))
}

/// Get the top table, allocating it on first use. Returns null only on OOM.
fn ensure_top() -> *mut TopEntry {
    let t = TOP.load(Ordering::Acquire);
    if !t.is_null() {
        return t;
    }
    let _g = INIT_LOCK.lock();
    let t = TOP.load(Ordering::Acquire);
    if !t.is_null() {
        return t;
    }
    let bytes = TOP_COUNT * core::mem::size_of::<TopEntry>();
    match os::alloc(bytes, true) {
        Some((p, _memid)) => {
            let base = p.as_ptr() as *mut TopEntry;
            // Point every entry at the shared zero-submap so a lookup never sees
            // a null submap (see [`ZERO_SUBMAP`]). One-time, before the table is
            // published, so no other thread can observe a half-filled table.
            let zs = zero_submap();
            for i in 0..TOP_COUNT {
                // SAFETY: `i < TOP_COUNT`; `base` is the freshly allocated table.
                unsafe { (*base.add(i)).store(zs, Ordering::Relaxed) };
            }
            TOP.store(base, Ordering::Release);
            base
        }
        None => core::ptr::null_mut(),
    }
}

/// Get (allocating if needed) the *real* submap for top index `top_idx`,
/// replacing the shared zero-submap on first use. Returns the zero-submap only
/// on OOM (so the caller can detect failure without a null check elsewhere).
fn ensure_submap(top: *mut TopEntry, top_idx: usize) -> *mut Submap {
    let zs = zero_submap();
    // SAFETY: `top_idx < TOP_COUNT` by construction; `top` points at the table.
    let entry = unsafe { &*top.add(top_idx) };
    let s = entry.load(Ordering::Acquire);
    if s != zs {
        return s; // already a real, dedicated submap
    }
    let _g = INIT_LOCK.lock();
    let s = entry.load(Ordering::Acquire);
    if s != zs {
        return s;
    }
    match os::alloc(core::mem::size_of::<Submap>(), true) {
        Some((p, _memid)) => {
            let sp = p.as_ptr() as *mut Submap;
            entry.store(sp, Ordering::Release);
            sp
        }
        None => zs,
    }
}

/// Record that the `slice_count` slices starting at `addr` belong to `page`.
///
/// Returns `false` on OOM (submap could not be allocated).
///
/// # Safety
/// `addr` must be slice-aligned and `page` a valid page pointer for that range.
pub unsafe fn register(addr: usize, slice_count: usize, page: *mut u8) -> bool {
    let top = ensure_top();
    if top.is_null() {
        return false;
    }
    let u0 = slice_index(addr);
    let zs = zero_submap();
    for s in 0..slice_count {
        let (top_idx, sub_idx) = split(u0 + s);
        let submap = if top_idx < TOP_COUNT {
            ensure_submap(top, top_idx)
        } else {
            zs
        };
        if submap == zs {
            // Out of range, or OOM allocating a submap. Roll back the entries
            // written so far so the mapping is all-or-nothing (no stale
            // half-mapping). SAFETY: slice-aligned range previously registered.
            unsafe { unregister(addr, s) };
            return false;
        }
        // SAFETY: `sub_idx < SUB_COUNT`; submap is a valid array.
        unsafe {
            (*submap)[sub_idx].store(page, Ordering::Release);
        }
    }
    true
}

/// Clear the mapping for `slice_count` slices starting at `addr`.
///
/// # Safety
/// `addr` must be slice-aligned to a previously registered range.
pub unsafe fn unregister(addr: usize, slice_count: usize) {
    let top = TOP.load(Ordering::Acquire);
    if top.is_null() {
        return;
    }
    let u0 = slice_index(addr);
    for s in 0..slice_count {
        let (top_idx, sub_idx) = split(u0 + s);
        if top_idx >= TOP_COUNT {
            return;
        }
        // SAFETY: top index in range.
        let entry = unsafe { &*top.add(top_idx) };
        let submap = entry.load(Ordering::Acquire);
        if submap == zero_submap() {
            // Never registered (still the shared read-only zero-submap) — and
            // we must never write into the shared submap.
            continue;
        }
        // SAFETY: sub index in range.
        unsafe {
            (*submap)[sub_idx].store(core::ptr::null_mut(), Ordering::Release);
        }
    }
}

/// Look up the page owning address `addr`, or null if unmapped.
///
/// Once the top table exists, every entry points at a valid submap (a real one
/// or the shared all-null [`ZERO_SUBMAP`]), so the hot path is two dependent
/// loads and **no submap-null branch** — an unmapped/foreign address resolves
/// to a null page through the zero-submap. Only the `TOP`-null guard (before the
/// first registration) and the canonical-range guard remain. Mirrors C's
/// 2-level `_mi_checked_ptr_page` (`internal.h`), minus the per-lookup
/// submap-null test that the zero-submap makes unnecessary.
#[inline]
pub fn lookup(addr: usize) -> *mut u8 {
    let top = TOP.load(Ordering::Acquire);
    if top.is_null() {
        return core::ptr::null_mut();
    }
    let (top_idx, sub_idx) = split(slice_index(addr));
    if top_idx >= TOP_COUNT {
        return core::ptr::null_mut();
    }
    // SAFETY: top index in range; `top` is the live table; every published entry
    // points at a valid submap (real or the shared zero-submap), never null.
    let submap = unsafe { (*top.add(top_idx)).load(Ordering::Acquire) };
    // SAFETY: sub index in range; `submap` is a valid `Submap`.
    unsafe { (*submap)[sub_idx].load(Ordering::Acquire) }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::bits::MI_ARENA_SLICE_SIZE;

    #[test]
    fn register_lookup_unregister() {
        // Use a real OS slice-aligned region so addresses are realistic.
        let (p, memid) =
            os::alloc_aligned(4 * MI_ARENA_SLICE_SIZE, MI_ARENA_SLICE_SIZE, true, false).unwrap();
        let base = p.addr().get();
        let fake_page = base as *mut u8; // page header would live here
                                         // SAFETY: region is slice-aligned and live.
        unsafe {
            assert!(register(base, 4, fake_page));
            // every address inside the 4 slices resolves to the page
            assert_eq!(lookup(base), fake_page);
            assert_eq!(lookup(base + 1234), fake_page);
            assert_eq!(lookup(base + 3 * MI_ARENA_SLICE_SIZE + 7), fake_page);
            // an address just past the region is unmapped
            assert!(lookup(base + 4 * MI_ARENA_SLICE_SIZE).is_null());
            unregister(base, 4);
            assert!(lookup(base).is_null());
            os::free(&memid);
        }
    }
}
