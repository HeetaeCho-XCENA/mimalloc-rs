// SPDX-License-Identifier: MIT
//! Allocation statistics (ports a subset of `src/stats.c`).
//!
//! Behind the `stats` feature this tracks process-wide allocation counts and
//! live/peak byte high-water marks. With the feature off every hook compiles to
//! nothing and the counters read as zero — zero cost on the hot path.
//!
//! Per-bin and per-arena breakdowns and OS RSS/commit sampling
//! (`_mi_prim_process_info`) remain follow-up work.

#[cfg(feature = "stats")]
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[cfg(feature = "stats")]
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "stats")]
static FREE_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "stats")]
static CURRENT_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "stats")]
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "stats")]
static PAGES_CREATED: AtomicU64 = AtomicU64::new(0);

/// Record an allocation of `size` usable bytes (no-op unless `stats`).
#[inline]
pub fn on_alloc(size: usize) {
    #[cfg(feature = "stats")]
    {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        let cur = CURRENT_BYTES.fetch_add(size, Ordering::Relaxed) + size;
        // Bump the peak high-water mark (a racy max is fine for a statistic).
        let mut peak = PEAK_BYTES.load(Ordering::Relaxed);
        while cur > peak {
            match PEAK_BYTES.compare_exchange_weak(peak, cur, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(p) => peak = p,
            }
        }
    }
    #[cfg(not(feature = "stats"))]
    let _ = size;
}

/// Record a free of `size` usable bytes (no-op unless `stats`).
#[inline]
pub fn on_free(size: usize) {
    #[cfg(feature = "stats")]
    {
        FREE_COUNT.fetch_add(1, Ordering::Relaxed);
        CURRENT_BYTES.fetch_sub(size, Ordering::Relaxed);
    }
    #[cfg(not(feature = "stats"))]
    let _ = size;
}

/// Record that a new page was carved from an arena (no-op unless `stats`).
#[inline]
pub fn on_page_created() {
    #[cfg(feature = "stats")]
    PAGES_CREATED.fetch_add(1, Ordering::Relaxed);
}

/// A snapshot of the process counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub allocations: u64,
    pub frees: u64,
    /// Currently-live usable bytes (allocated − freed).
    pub current_bytes: usize,
    /// Peak live usable bytes observed.
    pub peak_bytes: usize,
    /// Pages carved from arenas.
    pub pages_created: u64,
}

/// Read the current statistics (zeros unless `stats`).
pub fn snapshot() -> Stats {
    #[cfg(feature = "stats")]
    {
        Stats {
            allocations: ALLOC_COUNT.load(Ordering::Relaxed),
            frees: FREE_COUNT.load(Ordering::Relaxed),
            current_bytes: CURRENT_BYTES.load(Ordering::Relaxed),
            peak_bytes: PEAK_BYTES.load(Ordering::Relaxed),
            pages_created: PAGES_CREATED.load(Ordering::Relaxed),
        }
    }
    #[cfg(not(feature = "stats"))]
    {
        Stats::default()
    }
}

#[cfg(all(test, feature = "stats"))]
mod tests {
    use super::*;

    #[test]
    fn counters_track_alloc_free() {
        // Counters are process-global and other tests mutate them concurrently,
        // so only monotonic (`>=`) properties are asserted here.
        let before = snapshot();
        on_alloc(100);
        on_alloc(200);
        let mid = snapshot();
        assert!(mid.allocations >= before.allocations + 2);
        assert!(mid.peak_bytes >= mid.current_bytes);
        on_free(100);
        on_free(200);
        let after = snapshot();
        assert!(after.frees >= before.frees + 2);
    }
}
