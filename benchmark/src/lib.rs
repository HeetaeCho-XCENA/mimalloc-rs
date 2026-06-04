// SPDX-License-Identifier: MIT
//! Shared harness for the mimalloc-rs benchmark workloads (faithful Rust ports
//! of mimalloc-bench microbenchmarks).
//!
//! The workloads run with mimalloc-rs as the static `#[global_allocator]` — its
//! peak (fully inlined). Building with `--features bench-system` swaps in Rust's
//! `System` (glibc) allocator for the *same* binary, so the rs-vs-system
//! comparison is on identical code. (mimalloc-c is compared separately by
//! building the original C benchmark with `src/static.c -O3 -flto`.)

#[cfg(not(feature = "bench-system"))]
#[global_allocator]
static GLOBAL: mimalloc_rs::MiMalloc = mimalloc_rs::MiMalloc;

#[cfg(feature = "bench-system")]
#[global_allocator]
static GLOBAL: std::alloc::System = std::alloc::System;

use std::time::Instant;

/// The allocator this build links (for the banner).
pub fn alloc_name() -> &'static str {
    if cfg!(feature = "bench-system") {
        "system(glibc)"
    } else {
        "mimalloc-rs"
    }
}

/// splitmix64 — the deterministic PRNG mimalloc-bench uses for `pick`.
#[inline]
pub fn pick(r: &mut u64) -> u64 {
    let mut x = *r;
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^= x >> 31;
    *r = x;
    x
}

/// `true` with `perc`% probability (matches mimalloc-bench's `chance`).
#[inline]
pub fn chance(perc: u64, r: &mut u64) -> bool {
    pick(r) % 100 <= perc
}

/// Time `f` and print `BENCH <name> [<alloc>] SECONDS <s>` to stderr (so a
/// harness can `grep` it). The allocator name comes from the build feature.
pub fn timed(name: &str, f: impl FnOnce()) {
    let t = Instant::now();
    f();
    eprintln!(
        "BENCH {name} [{}] SECONDS {:.4}",
        alloc_name(),
        t.elapsed().as_secs_f64()
    );
}

/// Run `work(tid)` on `n` scoped threads, joined before returning.
pub fn run_threads<F: Fn(usize) + Sync>(n: usize, work: F) {
    std::thread::scope(|s| {
        for tid in 0..n {
            let w = &work;
            s.spawn(move || w(tid));
        }
    });
}

/// Parse positional integer args (1-based), falling back to `default`.
pub fn arg(i: usize, default: usize) -> usize {
    std::env::args()
        .nth(i)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
