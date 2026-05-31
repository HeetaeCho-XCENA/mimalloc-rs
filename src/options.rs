// SPDX-License-Identifier: MIT
//! Runtime options (ports a subset of `src/options.c`).
//!
//! mimalloc exposes ~30 `MIMALLOC_*` environment options. This port wires the
//! mechanism (parsed once at startup) and a representative set of options;
//! `eager_commit` is connected to arena reservation behavior, the rest are
//! parsed and exposed for callers/diagnostics. The full table is follow-up work.

use crate::prim::{DefaultPrim, Prim};
use crate::sync::OnceBox;

/// Parse a boolean env option (`1/y/t` ⇒ true), defaulting to `default`.
fn env_bool(name: &str, default: bool) -> bool {
    let mut buf = [0u8; 64];
    match DefaultPrim::getenv(name, &mut buf) {
        Some(n) if n > 0 => matches!(buf[0], b'1' | b'y' | b'Y' | b't' | b'T'),
        _ => default,
    }
}

/// Parse a signed-integer env option, defaulting to `default`.
fn env_long(name: &str, default: i64) -> i64 {
    let mut buf = [0u8; 64];
    match DefaultPrim::getenv(name, &mut buf) {
        Some(n) if n > 0 => parse_i64(&buf[..n]).unwrap_or(default),
        _ => default,
    }
}

fn parse_i64(b: &[u8]) -> Option<i64> {
    let (neg, digits) = match b.first() {
        Some(b'-') => (true, &b[1..]),
        _ => (false, b),
    };
    if digits.is_empty() {
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

/// The (lazily initialized) process options (mirrors `mi_option_t`).
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// `MIMALLOC_VERBOSE`: emit diagnostics to stderr.
    pub verbose: bool,
    /// `MIMALLOC_SHOW_STATS`: print stats at exit.
    pub show_stats: bool,
    /// `MIMALLOC_EAGER_COMMIT`: commit new arenas up front (vs commit-on-demand).
    /// Wired into arena reservation.
    pub eager_commit: bool,
    /// `MIMALLOC_PURGE_DECOMMITS`: purge by decommit (return RSS) vs reset.
    pub purge_decommits: bool,
    /// `MIMALLOC_PURGE_DELAY`: delay in ms before purging freed memory
    /// (`-1` disables purging). Parsed; delay-timer purging is follow-up.
    pub purge_delay: i64,
    /// `MIMALLOC_ARENA_RESERVE`: slices reserved when growing the arena pool.
    pub arena_reserve: i64,
}

/// Read the process options (parsed once from the environment).
pub fn options() -> &'static Options {
    static OPTS: OnceBox<Options> = OnceBox::new();
    OPTS.get_or_init(|| Options {
        verbose: env_bool("MIMALLOC_VERBOSE", false),
        show_stats: env_bool("MIMALLOC_SHOW_STATS", false),
        eager_commit: env_bool("MIMALLOC_EAGER_COMMIT", true),
        purge_decommits: env_bool("MIMALLOC_PURGE_DECOMMITS", false),
        purge_delay: env_long("MIMALLOC_PURGE_DELAY", 10),
        arena_reserve: env_long("MIMALLOC_ARENA_RESERVE", 0),
    })
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn options_stable_and_defaulted() {
        let a = options();
        let b = options();
        assert_eq!(a.verbose, b.verbose);
        assert!(a.eager_commit); // default true
    }

    #[test]
    fn parse_ints() {
        assert_eq!(parse_i64(b"42"), Some(42));
        assert_eq!(parse_i64(b"-7"), Some(-7));
        assert_eq!(parse_i64(b"10abc"), Some(10));
        assert_eq!(parse_i64(b""), None);
    }
}
