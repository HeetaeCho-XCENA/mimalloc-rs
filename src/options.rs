// SPDX-License-Identifier: MIT
//! Runtime options (ports a minimal subset of `src/options.c`).
//!
//! mimalloc exposes ~30 options read from `MIMALLOC_*` environment variables.
//! This port wires the mechanism (lazily read once via [`crate::prim`]) for a
//! couple of representative options; the full option table is follow-up work.

use crate::prim::{DefaultPrim, Prim};
use crate::sync::OnceBox;

/// A parsed boolean option, defaulting to `false`.
fn env_bool(name: &str) -> bool {
    let mut buf = [0u8; 64];
    match DefaultPrim::getenv(name, &mut buf) {
        Some(n) if n > 0 => matches!(buf[0], b'1' | b'y' | b'Y' | b't' | b'T'),
        _ => false,
    }
}

/// The (lazily initialized) process options.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// `MIMALLOC_VERBOSE`: emit diagnostics to stderr.
    pub verbose: bool,
    /// `MIMALLOC_SHOW_STATS`: print stats at exit.
    pub show_stats: bool,
}

/// Read the process options (parsed once from the environment).
pub fn options() -> &'static Options {
    static OPTS: OnceBox<Options> = OnceBox::new();
    OPTS.get_or_init(|| Options {
        verbose: env_bool("MIMALLOC_VERBOSE"),
        show_stats: env_bool("MIMALLOC_SHOW_STATS"),
    })
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn options_stable() {
        let a = options();
        let b = options();
        assert_eq!(a.verbose, b.verbose);
    }
}
