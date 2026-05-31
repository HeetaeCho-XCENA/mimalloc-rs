// SPDX-License-Identifier: MIT
//! Minimal `no_std` synchronization primitives used across the allocator.
//!
//! These exist so the core does not depend on `std`'s `OnceLock`/`Mutex`.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, Ordering};

const UNINIT: u8 = 0;
const INITIALIZING: u8 = 1;
const READY: u8 = 2;

/// A write-once cell that lazily initializes its value under contention.
///
/// Initialization runs exactly once; concurrent callers spin until it
/// completes. Suitable for process-global configuration computed at startup.
pub struct OnceBox<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: access to `value` is gated by the `state` atomic: it is only written
// once (by the thread that wins the UNINIT→INITIALIZING CAS) and only read
// after `state == READY` has been observed with `Acquire`, establishing a
// happens-before edge with the initializing thread's `Release` store.
unsafe impl<T: Send + Sync> Sync for OnceBox<T> {}

impl<T> OnceBox<T> {
    /// Create an empty cell.
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(UNINIT),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Return the value, initializing it with `f` on first call.
    pub fn get_or_init(&self, f: impl FnOnce() -> T) -> &T {
        if self.state.load(Ordering::Acquire) != READY {
            self.init_slow(f);
        }
        // SAFETY: state is READY here, so the value is initialized and the
        // Acquire load above synchronizes with the initializer's Release store.
        unsafe { (*self.value.get()).assume_init_ref() }
    }

    #[cold]
    fn init_slow(&self, f: impl FnOnce() -> T) {
        match self.state.compare_exchange(
            UNINIT,
            INITIALIZING,
            Ordering::Acquire,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                let v = f();
                // SAFETY: we hold the INITIALIZING token; no other thread reads
                // `value` until we publish READY.
                unsafe {
                    (*self.value.get()).write(v);
                }
                self.state.store(READY, Ordering::Release);
            }
            Err(_) => {
                // Another thread is initializing (or already did); wait.
                while self.state.load(Ordering::Acquire) != READY {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

impl<T> Default for OnceBox<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A tiny test-and-set spin lock for short, rarely-contended critical sections
/// (e.g. the metadata allocator and page-map submap allocation).
pub struct SpinLock {
    locked: AtomicU8,
}

impl SpinLock {
    pub const fn new() -> Self {
        Self {
            locked: AtomicU8::new(0),
        }
    }

    /// Acquire the lock, returning a guard that releases on drop.
    pub fn lock(&self) -> SpinGuard<'_> {
        while self
            .locked
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) != 0 {
                core::hint::spin_loop();
            }
        }
        SpinGuard { lock: self }
    }
}

impl Default for SpinLock {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard for [`SpinLock`].
pub struct SpinGuard<'a> {
    lock: &'a SpinLock,
}

impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        self.lock.locked.store(0, Ordering::Release);
    }
}
