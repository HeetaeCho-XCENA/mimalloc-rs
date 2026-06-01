// SPDX-License-Identifier: MIT
//! A focused, **mimalloc-rs-only** allocation hot loop for profiling.
//!
//! Unlike `bench_suite` (which interleaves system / C / rs across many phases),
//! this example runs nothing but mimalloc-rs's own small-object alloc/free hot
//! path, so a `perf record` / flamegraph contains only our code and the
//! hotspots are unambiguous. It does no comparison and prints almost nothing —
//! it exists to be profiled, not to report numbers (use `bench_suite` for that).
//!
//! Workload: keep a window of ~`WINDOW` live allocations, repeatedly allocating
//! a pseudo-random small size (the small fast path, `8..=1031` by default) and
//! freeing a random live block — the same shape as `bench_suite` phase 1, which
//! is where we trail the C reference.
//!
//! Tunables (env vars, all optional):
//!   `PROFILE_ROUNDS`  total alloc operations            (default 50_000_000)
//!   `PROFILE_WINDOW`  live allocations kept             (default 2048)
//!   `PROFILE_SIZE_MIN`/`PROFILE_SIZE_SPAN`  size range  (default 8 / 1024)
//!   `PROFILE_PIN_CORE` pin to this core (Linux)         (default 2)
//!
//! ## Profiling recipe (run on a quiet, pinned machine)
//!
//! Build once (release, with debug line info for symbol resolution):
//! ```sh
//! RUSTFLAGS="-C debuginfo=1" cargo build --release --example profile_alloc
//! ```
//!
//! Flamegraph (needs `cargo install flamegraph` + perf):
//! ```sh
//! cargo flamegraph --release --example profile_alloc        # → flamegraph.svg
//! ```
//!
//! Raw perf with call graphs, then a symbol breakdown:
//! ```sh
//! perf record -g --call-graph=dwarf -- \
//!   ./target/release/examples/profile_alloc
//! perf report --stdio | head -40        # top self-time symbols
//! ```
//!
//! Counter view — distinguishes a *division/compute*-bound path from a
//! *TLS/cache-miss*-bound one (low IPC + high dTLB/branch misses ⇒ memory; high
//! IPC + high instructions ⇒ compute like the free-path divide):
//! ```sh
//! perf stat -d -- ./target/release/examples/profile_alloc
//! # watch: instructions per cycle (IPC), branch-misses, dTLB-load-misses
//! ```
//!
//! To attribute cost between alloc and free, build two variants by flipping
//! `FREE_HALF` (below) or just read the flamegraph: `Heap::alloc*`/`Page::alloc`
//! vs `heap::free`/`page_map::lookup` split the tree.

use mimalloc_rs::MiMalloc;
use std::alloc::{GlobalAlloc, Layout};
use std::hint::black_box;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[cfg(target_os = "linux")]
fn pin_to_core(core: usize) -> bool {
    // SAFETY: zeroed cpu_set_t is valid; CPU_SET sets one in-range bit;
    // sched_setaffinity(0, ..) targets the current thread.
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

#[cfg(not(target_os = "linux"))]
fn pin_to_core(_core: usize) -> bool {
    false
}

fn main() {
    let rounds = env_usize("PROFILE_ROUNDS", 50_000_000);
    let window = env_usize("PROFILE_WINDOW", 2048);
    let size_min = env_usize("PROFILE_SIZE_MIN", 8);
    let size_span = env_usize("PROFILE_SIZE_SPAN", 1024);
    let pin_core = env_usize("PROFILE_PIN_CORE", 2);
    let pinned = pin_to_core(pin_core);

    let a = MiMalloc;
    let mut live: Vec<(*mut u8, Layout)> = Vec::with_capacity(window + 1);
    let mut x: u64 = 0x1234_5678_9abc_def1;
    let mut checksum: u64 = 0;

    // The hot loop. Identical shape to `bench_suite` phase 1, mimalloc-rs only.
    for _ in 0..rounds {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let sz = size_min + (x as usize % size_span);
        let layout = Layout::from_size_align(sz, 16).unwrap();
        // SAFETY: standard GlobalAlloc usage with matching layouts.
        unsafe {
            let p = a.alloc(layout);
            if !p.is_null() {
                *p = 1; // touch
                checksum = checksum.wrapping_add(p as u64);
                live.push((p, layout));
            }
            if live.len() > window {
                let idx = (x as usize) % live.len();
                let (q, ql) = live.swap_remove(idx);
                a.dealloc(q, ql);
            }
        }
    }
    // SAFETY: drain remaining live allocations.
    for (q, ql) in live {
        unsafe { a.dealloc(q, ql) };
    }

    black_box(checksum);
    eprintln!(
        "profile_alloc: {rounds} ops, window {window}, sizes {size_min}..{}, pinned={pinned}",
        size_min + size_span
    );
}
