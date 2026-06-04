// SPDX-License-Identifier: MIT
//! Public API surface (ports `mimalloc.h`'s `mi_*` functions and the override
//! adapters).
//!
//! * [`MiMalloc`] — `GlobalAlloc` + `Allocator` handle (submodules
//!   [`global_alloc`], [`allocator`]).
//! * `mi_*` — C-style raw-pointer entry points mirroring the original API.

mod allocator;
pub mod global_alloc;

pub use global_alloc::MiMalloc;

/// C-style `mi_*` entry points (raw pointers, null on failure).
///
/// These mirror `mimalloc.h`. They use the calling thread's default heap, so
/// they require the `std` feature; `no_std` embedders use [`crate::Heap`].
#[cfg(feature = "std")]
pub mod mi {
    use crate::heap;
    use crate::init;
    use core::ptr::NonNull;

    /// `mi_malloc`: allocate `size` bytes (null on failure).
    #[inline]
    pub fn mi_malloc(size: usize) -> *mut u8 {
        init::malloc(size).map_or(core::ptr::null_mut(), |p| p.as_ptr())
    }

    /// `mi_zalloc`: allocate `size` zeroed bytes.
    #[inline]
    pub fn mi_zalloc(size: usize) -> *mut u8 {
        init::zalloc(size).map_or(core::ptr::null_mut(), |p| p.as_ptr())
    }

    /// `mi_calloc`: allocate `count * size` zeroed bytes (overflow ⇒ null).
    #[inline]
    pub fn mi_calloc(count: usize, size: usize) -> *mut u8 {
        match count.checked_mul(size) {
            Some(total) => mi_zalloc(total),
            None => core::ptr::null_mut(),
        }
    }

    /// `mi_malloc_aligned`: allocate `size` bytes aligned to `alignment`.
    #[inline]
    pub fn mi_malloc_aligned(size: usize, alignment: usize) -> *mut u8 {
        if !alignment.is_power_of_two() {
            return core::ptr::null_mut();
        }
        init::malloc_aligned(size, alignment).map_or(core::ptr::null_mut(), |p| p.as_ptr())
    }

    /// `mi_realloc`: resize `p` to `new_size`.
    ///
    /// # Safety
    /// `p` is null or a live allocation from this allocator.
    pub unsafe fn mi_realloc(p: *mut u8, new_size: usize) -> *mut u8 {
        match NonNull::new(p) {
            None => mi_malloc(new_size),
            // SAFETY: `p` is a live allocation per contract.
            Some(nn) => unsafe {
                init::realloc(nn, new_size).map_or(core::ptr::null_mut(), |q| q.as_ptr())
            },
        }
    }

    /// `mi_free`: free `p` (null is a no-op).
    ///
    /// # Safety
    /// `p` is null or a live allocation from this allocator.
    pub unsafe fn mi_free(p: *mut u8) {
        if let Some(nn) = NonNull::new(p) {
            // SAFETY: `p` is a live allocation per contract.
            unsafe { init::free(nn) }
        }
    }

    /// `mi_usable_size`: the usable size of allocation `p` (0 if null/unknown).
    ///
    /// # Safety
    /// `p` is null or a live allocation from this allocator.
    pub unsafe fn mi_usable_size(p: *mut u8) -> usize {
        match NonNull::new(p) {
            None => 0,
            // SAFETY: live allocation per contract.
            Some(nn) => unsafe { heap::usable_size(nn) },
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::mi::*;
    use super::MiMalloc;
    use core::alloc::{GlobalAlloc, Layout};

    #[test]
    fn mi_c_api_roundtrip() {
        // SAFETY: standard C-style use.
        unsafe {
            let p = mi_malloc(64);
            assert!(!p.is_null());
            core::ptr::write_bytes(p, 0xCC, 64);
            assert!(mi_usable_size(p) >= 64);
            let p2 = mi_realloc(p, 4096);
            assert!(!p2.is_null());
            // preserved first byte
            assert_eq!(*p2, 0xCC);
            mi_free(p2);

            let z = mi_zalloc(32);
            assert_eq!(*z, 0);
            mi_free(z);

            let a = mi_malloc_aligned(40, 64);
            assert_eq!(a.addr() % 64, 0);
            mi_free(a);

            assert!(mi_calloc(usize::MAX, 2).is_null()); // overflow guarded
        }
    }

    #[test]
    fn global_alloc_impl() {
        let mm = MiMalloc;
        // SAFETY: standard GlobalAlloc use with matching layouts.
        unsafe {
            let l = Layout::from_size_align(1000, 32).unwrap();
            let p = mm.alloc(l);
            assert!(!p.is_null());
            assert_eq!(p.addr() % 32, 0);
            core::ptr::write_bytes(p, 0xAB, 1000);
            let p2 = mm.realloc(p, l, 8000);
            assert!(!p2.is_null());
            assert_eq!(*p2, 0xAB);
            mm.dealloc(p2, Layout::from_size_align(8000, 32).unwrap());

            let z = mm.alloc_zeroed(Layout::from_size_align(128, 16).unwrap());
            assert_eq!(*z, 0);
            mm.dealloc(z, Layout::from_size_align(128, 16).unwrap());
        }
    }

    // The arena allocation cursor starts the slice search at the last-used
    // (frontier) arena and wraps. Allocate enough huge blocks to span several
    // arenas, free a scattered subset, then re-allocate the same count: the wrap
    // must still reclaim the freed slots (no leak, no overlap with live blocks).
    #[test]
    fn arena_cursor_reclaims_freed_across_arenas() {
        let mm = MiMalloc;
        let size = 2 * 1024 * 1024; // huge; ~130 of these exceed one 256 MiB arena
        let l = Layout::from_size_align(size, 16).unwrap();
        // SAFETY: matched alloc/dealloc with one layout throughout.
        unsafe {
            let mut live: Vec<*mut u8> = (0..200).map(|_| mm.alloc(l)).collect();
            assert!(live.iter().all(|p| !p.is_null()));
            // Free every third block (holes scattered below the frontier).
            for i in (0..live.len()).step_by(3) {
                mm.dealloc(live[i], l);
                live[i] = core::ptr::null_mut();
            }
            // Re-allocate as many; these must reuse the freed slots.
            for slot in live.iter_mut().filter(|p| p.is_null()) {
                *slot = mm.alloc(l);
                assert!(!slot.is_null());
            }
            // All live pointers distinct (no double-hand-out).
            let mut addrs: Vec<usize> = live.iter().map(|p| *p as usize).collect();
            addrs.sort_unstable();
            let n = addrs.len();
            addrs.dedup();
            assert_eq!(
                addrs.len(),
                n,
                "arena cursor handed out an overlapping block"
            );
            for p in live {
                mm.dealloc(p, l);
            }
        }
    }

    // The large/huge `alloc_zeroed` fast path skips the body memset when the
    // serving slices are OS-zero (clearing only the free-list link word). Churn
    // a large block through dirty→free→purge→reuse and require every byte zero
    // each round — a reused-but-dirty block wrongly flagged zero would fail here.
    #[test]
    fn alloc_zeroed_large_stays_zero_across_reuse() {
        let mm = MiMalloc;
        let size = 2 * 1024 * 1024; // > MI_MEDIUM_MAX_OBJ_SIZE: the page fast path
        let l = Layout::from_size_align(size, 16).unwrap();
        // SAFETY: matched alloc_zeroed / dealloc with one layout.
        unsafe {
            for _ in 0..32 {
                let p = mm.alloc_zeroed(l);
                assert!(!p.is_null());
                let s = core::slice::from_raw_parts(p, size);
                assert!(s.iter().all(|&b| b == 0), "alloc_zeroed returned non-zero");
                core::ptr::write_bytes(p, 0xFF, size); // dirty before freeing
                mm.dealloc(p, l);
                crate::init::collect(true); // force the delayed purge to run
            }
        }
    }

    // Huge pages keep their header off the data slice (init_huge) and the block
    // is served directly. Exercise write/read integrity at both ends of the
    // block, plus many alloc/free cycles (retire → meta_free + page-map
    // register/unregister round-trip) to catch any UAF/leak in that path.
    #[test]
    fn huge_offslice_alloc_free_integrity() {
        let mm = MiMalloc;
        let size = 3 * 1024 * 1024; // huge (> 512 KiB)
        let l = Layout::from_size_align(size, 16).unwrap();
        // SAFETY: matched alloc/dealloc with one layout.
        unsafe {
            for k in 0..64u8 {
                let p = mm.alloc(l);
                assert!(!p.is_null());
                // Write a pattern at the first and last byte; the header is
                // off-slice, so neither must corrupt metadata nor be lost.
                *p = k;
                *p.add(size - 1) = k ^ 0xFF;
                assert_eq!(*p, k);
                assert_eq!(*p.add(size - 1), k ^ 0xFF);
                mm.dealloc(p, l);
            }
        }
    }

    #[test]
    fn allocator_api2_box_and_vec() {
        use allocator_api2::boxed::Box;
        use allocator_api2::vec::Vec;
        let b = Box::new_in(0xDEAD_BEEFu64, MiMalloc);
        assert_eq!(*b, 0xDEAD_BEEF);
        let mut v: Vec<u32, MiMalloc> = Vec::new_in(MiMalloc);
        for i in 0..1000u32 {
            v.push(i);
        }
        assert_eq!(v.iter().copied().sum::<u32>(), (0..1000).sum());
        // both drop here, returning memory to the allocator
    }
}
