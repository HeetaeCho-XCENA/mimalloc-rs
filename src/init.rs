// SPDX-License-Identifier: MIT
//! Process/thread initialization and the thread-local default heap
//! (ports the lifecycle core of `src/init.c`).
//!
//! v1 bootstraps lazily: process-wide free-list encoding keys are computed once
//! (from OS randomness), and each thread gets its own [`Heap`] in thread-local
//! storage on first use. The richer lifecycle — `pthread_key` thread-exit page
//! handoff, reentrancy guards for `#[global_allocator]` init-before-main — is
//! introduced alongside cross-thread free in M6/M7.
//!
//! The thread-local default heap requires the `std` feature; `no_std` embedders
//! drive their own [`Heap`] instances directly.

use crate::prim::{DefaultPrim, Prim};
use crate::sync::OnceBox;

/// Process-wide free-list encoding keys (random, computed once).
pub fn process_keys() -> [usize; 2] {
    static KEYS: OnceBox<[usize; 2]> = OnceBox::new();
    *KEYS.get_or_init(|| {
        let mut buf = [0u8; 2 * core::mem::size_of::<usize>()];
        if DefaultPrim::random_buf(&mut buf) {
            let mut k = [0usize; 2];
            let w = core::mem::size_of::<usize>();
            let mut b0 = [0u8; core::mem::size_of::<usize>()];
            let mut b1 = [0u8; core::mem::size_of::<usize>()];
            b0.copy_from_slice(&buf[..w]);
            b1.copy_from_slice(&buf[w..2 * w]);
            k[0] = usize::from_ne_bytes(b0);
            k[1] = usize::from_ne_bytes(b1);
            k
        } else {
            // Deterministic fallback if the OS RNG is unavailable.
            [0x9e37_79b9_7f4a_7c15, 0xc2b2_ae3d_27d4_eb4f]
        }
    })
}

/// A unique, stable id for the calling thread (low 2 bits clear, non-zero) used
/// to stamp page ownership and route cross-thread frees.
#[cfg(feature = "std")]
pub fn current_tid() -> usize {
    // Plain process-global counter (not part of any modeled concurrency), so we
    // use core atomics directly — they are usable in `static` (and under loom).
    use core::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    std::thread_local! {
        static TID: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }
    TID.with(|t| {
        let v = t.get();
        if v != 0 {
            v
        } else {
            // shift left by 2 so the low bits stay free for page flags
            let id = NEXT.fetch_add(1, Ordering::Relaxed) << 2;
            t.set(id);
            id
        }
    })
}

#[cfg(feature = "std")]
mod tls {
    use super::{current_tid, process_keys};
    use crate::heap::Heap;
    use core::ptr::NonNull;

    std::thread_local! {
        /// The calling thread's default heap.
        static DEFAULT_HEAP: Heap = Heap::new(process_keys(), current_tid());
    }

    /// Allocate `size` bytes from the calling thread's default heap.
    #[inline]
    pub fn malloc(size: usize) -> Option<NonNull<u8>> {
        DEFAULT_HEAP.with(|h| h.alloc(size))
    }

    /// Allocate `size` bytes aligned to `align` from the default heap.
    #[inline]
    pub fn malloc_aligned(size: usize, align: usize) -> Option<NonNull<u8>> {
        DEFAULT_HEAP.with(|h| h.alloc_aligned(size, align))
    }

    /// Allocate zeroed memory of `size` bytes.
    #[inline]
    pub fn zalloc(size: usize) -> Option<NonNull<u8>> {
        let p = DEFAULT_HEAP.with(|h| h.alloc(size))?;
        // SAFETY: `p` points to at least `size` writable bytes.
        unsafe {
            core::ptr::write_bytes(p.as_ptr(), 0, size);
        }
        Some(p)
    }
}

#[cfg(feature = "std")]
pub use tls::{malloc, malloc_aligned, zalloc};

/// Grow/shrink an allocation, preserving its contents.
///
/// # Safety
/// `ptr` must be a live allocation from this allocator.
#[cfg(feature = "std")]
pub unsafe fn realloc(
    ptr: core::ptr::NonNull<u8>,
    new_size: usize,
) -> Option<core::ptr::NonNull<u8>> {
    // SAFETY: ptr is a live allocation.
    let old = unsafe { crate::heap::usable_size(ptr) };
    if new_size <= old {
        return Some(ptr);
    }
    let np = malloc(new_size)?;
    // SAFETY: both regions are valid for `min(old, new_size)` bytes and disjoint.
    unsafe {
        core::ptr::copy_nonoverlapping(ptr.as_ptr(), np.as_ptr(), old.min(new_size));
        free(ptr);
    }
    Some(np)
}

/// Same as [`realloc`] but with an explicit alignment for the new block.
///
/// # Safety
/// `ptr` must be a live allocation from this allocator.
#[cfg(feature = "std")]
pub unsafe fn realloc_aligned(
    ptr: core::ptr::NonNull<u8>,
    new_size: usize,
    align: usize,
) -> Option<core::ptr::NonNull<u8>> {
    // SAFETY: ptr is a live allocation.
    let old = unsafe { crate::heap::usable_size(ptr) };
    let np = malloc_aligned(new_size, align)?;
    // SAFETY: valid, disjoint regions.
    unsafe {
        core::ptr::copy_nonoverlapping(ptr.as_ptr(), np.as_ptr(), old.min(new_size));
        free(ptr);
    }
    Some(np)
}

/// Free a pointer obtained from [`malloc`]/[`zalloc`].
///
/// # Safety
/// `ptr` must be a live allocation from this allocator.
#[inline]
pub unsafe fn free(ptr: core::ptr::NonNull<u8>) {
    // SAFETY: forwarded contract.
    unsafe { crate::heap::free(ptr) }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn process_keys_stable() {
        let a = process_keys();
        let b = process_keys();
        assert_eq!(a, b, "keys must be stable across calls");
    }

    #[test]
    fn default_heap_malloc_free() {
        // SAFETY: pointers come from this allocator.
        unsafe {
            let p = malloc(123).unwrap();
            core::ptr::write_bytes(p.as_ptr(), 0x42, 123);
            assert_eq!(*p.as_ptr(), 0x42);
            free(p);

            let z = zalloc(64).unwrap();
            for i in 0..64 {
                assert_eq!(*z.as_ptr().add(i), 0, "zalloc must zero");
            }
            free(z);
        }
    }

    #[test]
    fn multithreaded_each_thread_own_heap() {
        // Each thread allocates and frees its own pointers (no cross-thread free
        // yet — that is M6). Verifies per-thread TLS heaps work concurrently.
        let handles: alloc::vec::Vec<_> = (0..8)
            .map(|t| {
                std::thread::spawn(move || {
                    // SAFETY: each thread frees only what it allocated.
                    unsafe {
                        let mut v = alloc::vec::Vec::new();
                        for i in 0..2000usize {
                            let p = malloc(8 + (i % 200)).unwrap();
                            core::ptr::write_bytes(p.as_ptr(), t as u8, 8);
                            v.push(p);
                        }
                        for p in v {
                            free(p);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    extern crate alloc;
}
