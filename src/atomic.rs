// SPDX-License-Identifier: MIT
//! Atomic types and the memory-ordering conventions used across the allocator
//! (mirrors `include/mimalloc/atomic.h`).
//!
//! The orderings follow mimalloc exactly on the hot paths: relaxed reads in
//! CAS retry loops, acquire/release on lock-free publish/observe, and
//! acquire-release (full barrier) compare-exchanges for ownership transitions
//! (`free.c` `xthread_free`, `bitmap.c` field CAS).
//!
//! Under `--cfg loom` the atomic types resolve to `loom`'s instrumented atomics
//! so the concurrency models in the test suite explore all interleavings.

#[cfg(loom)]
pub use loom::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

#[cfg(not(loom))]
pub use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

/// Acquire-load.
#[inline]
pub fn load_acquire(a: &AtomicUsize) -> usize {
    a.load(Ordering::Acquire)
}

/// Relaxed load (for CAS retry loops).
#[inline]
pub fn load_relaxed(a: &AtomicUsize) -> usize {
    a.load(Ordering::Relaxed)
}

/// Release-store.
#[inline]
pub fn store_release(a: &AtomicUsize, v: usize) {
    a.store(v, Ordering::Release);
}

/// Weak compare-exchange with acquire-release success / acquire failure
/// (mimalloc `mi_atomic_cas_weak_acq_rel`).
#[inline]
pub fn cas_weak_acq_rel(a: &AtomicUsize, expected: usize, desired: usize) -> Result<usize, usize> {
    a.compare_exchange_weak(expected, desired, Ordering::AcqRel, Ordering::Acquire)
}

/// Strong compare-exchange with acquire-release success / acquire failure.
#[inline]
pub fn cas_strong_acq_rel(
    a: &AtomicUsize,
    expected: usize,
    desired: usize,
) -> Result<usize, usize> {
    a.compare_exchange(expected, desired, Ordering::AcqRel, Ordering::Acquire)
}
