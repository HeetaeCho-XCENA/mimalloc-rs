// SPDX-License-Identifier: MIT
//! Allocation statistics (ports a minimal subset of `src/stats.c`).
//!
//! Full per-bin/per-arena statistics are follow-up work. Behind the `stats`
//! feature this tracks process-wide allocate/free counts; with the feature off
//! the counters compile to nothing and the hooks are zero-cost no-ops.

#[cfg(feature = "stats")]
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "stats")]
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "stats")]
static FREE_COUNT: AtomicU64 = AtomicU64::new(0);

/// Record one allocation (no-op unless `stats`).
#[inline]
pub fn on_alloc() {
    #[cfg(feature = "stats")]
    ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Record one free (no-op unless `stats`).
#[inline]
pub fn on_free() {
    #[cfg(feature = "stats")]
    FREE_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// A snapshot of the process counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub allocations: u64,
    pub frees: u64,
}

/// Read the current statistics (zeros unless `stats`).
pub fn snapshot() -> Stats {
    #[cfg(feature = "stats")]
    {
        Stats {
            allocations: ALLOC_COUNT.load(Ordering::Relaxed),
            frees: FREE_COUNT.load(Ordering::Relaxed),
        }
    }
    #[cfg(not(feature = "stats"))]
    {
        Stats::default()
    }
}
