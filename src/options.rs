// SPDX-License-Identifier: MIT
//! Runtime options (ports a subset of `src/options.c`).
//!
//! Options are seeded once from `MIMALLOC_*` environment variables and are
//! **runtime-settable** (atomic), backing the `mi_option_*` C API. A
//! representative set is wired to behavior (`eager_commit` → arena reservation);
//! the rest are stored and exposed. The full ~30-option table is follow-up work.

use core::sync::atomic::{AtomicI64, Ordering};

use crate::prim::{DefaultPrim, Prim};
use crate::sync::OnceBox;

/// Option identifiers (a subset of `mi_option_t`; values are the C API indices).
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Opt {
    /// `MIMALLOC_VERBOSE`: emit diagnostics.
    Verbose = 0,
    /// `MIMALLOC_SHOW_STATS`: print stats at exit.
    ShowStats = 1,
    /// `MIMALLOC_EAGER_COMMIT`: commit new arenas up front (wired).
    EagerCommit = 2,
    /// `MIMALLOC_PURGE_DECOMMITS`: purge by decommit vs reset.
    PurgeDecommits = 3,
    /// `MIMALLOC_PURGE_DELAY`: ms before purging freed memory (`-1` disables).
    PurgeDelay = 4,
    /// `MIMALLOC_ARENA_RESERVE`: slices reserved when growing the pool.
    ArenaReserve = 5,
    /// `MIMALLOC_ARENA_PURGE_MULT`: multiplier on `PurgeDelay` for arenas (v3).
    ArenaPurgeMult = 6,
    /// `MIMALLOC_PAGE_FULL_RETAIN`: number of full (small) pages to keep in a
    /// bin queue before evicting (abandoning) them during the page search (v3).
    PageFullRetain = 7,
}

/// Number of options.
pub const OPT_COUNT: usize = 8;

impl Opt {
    /// Map a C API option index to an `Opt`.
    pub fn from_index(i: i32) -> Option<Opt> {
        match i {
            0 => Some(Opt::Verbose),
            1 => Some(Opt::ShowStats),
            2 => Some(Opt::EagerCommit),
            3 => Some(Opt::PurgeDecommits),
            4 => Some(Opt::PurgeDelay),
            5 => Some(Opt::ArenaReserve),
            6 => Some(Opt::ArenaPurgeMult),
            7 => Some(Opt::PageFullRetain),
            _ => None,
        }
    }

    fn env_name(self) -> &'static str {
        match self {
            Opt::Verbose => "MIMALLOC_VERBOSE",
            Opt::ShowStats => "MIMALLOC_SHOW_STATS",
            Opt::EagerCommit => "MIMALLOC_EAGER_COMMIT",
            Opt::PurgeDecommits => "MIMALLOC_PURGE_DECOMMITS",
            Opt::PurgeDelay => "MIMALLOC_PURGE_DELAY",
            Opt::ArenaReserve => "MIMALLOC_ARENA_RESERVE",
            Opt::ArenaPurgeMult => "MIMALLOC_ARENA_PURGE_MULT",
            Opt::PageFullRetain => "MIMALLOC_PAGE_FULL_RETAIN",
        }
    }

    fn default_value(self) -> i64 {
        // Mirror v3 `src/options.c` defaults for the wired options.
        match self {
            Opt::EagerCommit => 1,
            Opt::PurgeDecommits => 1, // v3: purge via decommit (MADV_DONTNEED on Linux)
            Opt::PurgeDelay => 1000,  // v3: 1000 ms before purging freed memory
            Opt::ArenaPurgeMult => 1, // v3: arena delay = purge_delay * 1
            // Full-page eviction default. `>=0` enables it with that retain
            // budget; `<0` disables it. It relieves cross-thread-free contention
            // (xmalloc-test) but its abandon/reclaim churn regresses the
            // single/intra-thread small-alloc path (perf_compare phase 1/2) where
            // there is nothing to relieve — so it is **off** for a static
            // `#[global_allocator]` and **on** for the preload `cdylib`
            // (`override_export`), the LD_PRELOAD allocator-replacement scenario.
            // The on-value is 16, not v3's 2: our abandon/reclaim churn is
            // costlier than C's, so a measured-higher retain keeps the
            // xmalloc-test win (-24%->-11%) without regressing larson/mstress
            // (docs/GOAL-fe2-revival.md FR3).
            #[cfg(override_export)]
            Opt::PageFullRetain => 16,
            #[cfg(not(override_export))]
            Opt::PageFullRetain => -1,
            _ => 0,
        }
    }
}

static VALUES: [AtomicI64; OPT_COUNT] = [const { AtomicI64::new(0) }; OPT_COUNT];
static INIT: OnceBox<()> = OnceBox::new();

/// Serializes tests that mutate the process-global option atomics so they do
/// not race each other under the parallel test runner.
#[cfg(all(test, feature = "std"))]
pub(crate) static OPTION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn parse_i64(b: &[u8]) -> Option<i64> {
    let (neg, digits) = match b.first() {
        Some(b'-') => (true, &b[1..]),
        _ => (false, b),
    };
    if digits.is_empty() || !digits[0].is_ascii_digit() {
        return None;
    }
    let mut v: i64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            break;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as i64)?;
    }
    Some(if neg { -v } else { v })
}

/// Read an option's value from the environment, or its default.
fn env_value(opt: Opt) -> i64 {
    let mut buf = [0u8; 64];
    match DefaultPrim::getenv(opt.env_name(), &mut buf) {
        Some(n) if n > 0 => {
            let b = &buf[..n];
            if b[0].is_ascii_digit() || b[0] == b'-' {
                parse_i64(b).unwrap_or_else(|| opt.default_value())
            } else if matches!(b[0], b'y' | b'Y' | b't' | b'T') {
                1
            } else {
                0
            }
        }
        _ => opt.default_value(),
    }
}

fn ensure_init() {
    INIT.get_or_init(|| {
        for (i, slot) in VALUES.iter().enumerate() {
            if let Some(opt) = Opt::from_index(i as i32) {
                slot.store(env_value(opt), Ordering::Relaxed);
            }
        }
    });
}

/// Get an option's current value.
pub fn get(opt: Opt) -> i64 {
    ensure_init();
    VALUES[opt as usize].load(Ordering::Relaxed)
}

/// Set an option's value at runtime.
pub fn set(opt: Opt, value: i64) {
    ensure_init();
    VALUES[opt as usize].store(value, Ordering::Relaxed);
}

/// Is a (boolean) option enabled (non-zero)?
pub fn is_enabled(opt: Opt) -> bool {
    get(opt) != 0
}

/// Enable a boolean option.
pub fn enable(opt: Opt) {
    set(opt, 1);
}

/// Disable a boolean option.
pub fn disable(opt: Opt) {
    set(opt, 0);
}

/// Convenience: whether new arenas are committed eagerly (wired into arenas).
#[inline]
pub fn eager_commit() -> bool {
    is_enabled(Opt::EagerCommit)
}

/// Purge delay in milliseconds: `<0` disables purging, `0` = purge immediately.
#[inline]
pub fn purge_delay() -> i64 {
    get(Opt::PurgeDelay)
}

/// Number of full (small) pages a bin queue retains before the page search
/// evicts (abandons) them (v3 `page_full_retain`, default 2). `<0` disables
/// eviction (full pages are never abandoned during the search).
#[inline]
pub fn page_full_retain() -> i64 {
    get(Opt::PageFullRetain)
}

/// Whether a purge returns memory via decommit (`true`) or reset (`false`).
#[inline]
pub fn purge_decommits() -> bool {
    is_enabled(Opt::PurgeDecommits)
}

/// Effective arena purge delay (ms) = `purge_delay * arena_purge_mult` (v3
/// `mi_arena_purge_delay`). Stays `<0` (disabled) when `purge_delay < 0`; the
/// multiplier is clamped to `>= 1` so a stray `0` cannot make purging immediate.
#[inline]
pub fn arena_purge_delay() -> i64 {
    purge_delay().saturating_mul(get(Opt::ArenaPurgeMult).max(1))
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_runtime_set() {
        let _g = OPTION_TEST_LOCK.lock().unwrap();
        assert!(is_enabled(Opt::EagerCommit)); // default on
        let prev = get(Opt::PurgeDelay);
        set(Opt::PurgeDelay, 42);
        assert_eq!(get(Opt::PurgeDelay), 42);
        set(Opt::PurgeDelay, prev); // restore (shared global)

        disable(Opt::Verbose);
        assert!(!is_enabled(Opt::Verbose));
        enable(Opt::Verbose);
        assert!(is_enabled(Opt::Verbose));
        disable(Opt::Verbose);

        assert_eq!(Opt::from_index(2), Some(Opt::EagerCommit));
        assert_eq!(Opt::from_index(6), Some(Opt::ArenaPurgeMult));
        assert_eq!(Opt::from_index(99), None);
    }

    #[test]
    fn purge_defaults_match_v3() {
        // Pure defaults (no env/global), so this is race-free.
        assert_eq!(Opt::PurgeDelay.default_value(), 1000);
        assert_eq!(Opt::PurgeDecommits.default_value(), 1);
        assert_eq!(Opt::ArenaPurgeMult.default_value(), 1);
        assert_eq!(Opt::EagerCommit.default_value(), 1);
    }

    #[test]
    fn parse_ints() {
        assert_eq!(parse_i64(b"42"), Some(42));
        assert_eq!(parse_i64(b"-7"), Some(-7));
        assert_eq!(parse_i64(b"y"), None);
        assert_eq!(parse_i64(b""), None);
    }
}
