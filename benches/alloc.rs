// SPDX-License-Identifier: MIT
//! Criterion statistical micro-benchmarks for the mimalloc-rs allocator.
//!
//! Run:
//! ```sh
//! cargo bench --bench alloc
//! cargo bench --bench alloc -- --save-baseline before   # record a baseline
//! cargo bench --bench alloc -- --baseline before        # compare against it
//! ```
//! These measure the allocator's own alloc/free cost across size classes and a
//! few realistic patterns, with mean/median/CI/outlier stats — good for *local*
//! micro-regression tracking on one machine. For end-to-end comparison against
//! the C reference / system allocator, use `examples/bench_suite.rs` +
//! `scripts/perf_compare.sh` (pinned-machine, authoritative) and
//! `scripts/mimalloc-bench.sh` (the standard cross-allocator suite).

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use mimalloc_rs::{free, malloc};
use std::ptr::NonNull;

/// Sizes spanning every bin kind: small/medium, large (≥16 KiB), huge (≥512 KiB).
const SIZES: &[usize] = &[
    8,
    64,
    256,
    1024,
    4096,
    16 * 1024,
    64 * 1024,
    512 * 1024,
    1024 * 1024,
];

/// One alloc + immediate free per size class (raw latency of a round trip).
fn alloc_free(c: &mut Criterion) {
    let mut g = c.benchmark_group("alloc_free");
    for &size in SIZES {
        g.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let p = malloc(black_box(size)).expect("oom");
                // Touch the first byte so the page is genuinely resident.
                // SAFETY: `p` is a fresh allocation of `size >= 1` bytes.
                unsafe { *p.as_ptr() = 1 };
                // SAFETY: `p` came from `malloc` and is freed exactly once.
                unsafe { free(p) };
            });
        });
    }
    g.finish();
}

/// Steady-state reuse: a power-of-two ring of `N` live blocks; each iteration
/// frees a slot's occupant and allocates a new block into it (the common
/// long-running pattern where the allocator recycles freed blocks).
fn ring(c: &mut Criterion) {
    const N: usize = 1024;
    let mut g = c.benchmark_group("ring");
    for &size in &[16usize, 64, 1024] {
        g.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let mut slots: Vec<*mut u8> = vec![core::ptr::null_mut(); N];
            let mut i = 0usize;
            b.iter(|| {
                let s = i & (N - 1);
                if !slots[s].is_null() {
                    // SAFETY: a non-null slot holds a block we allocated below.
                    unsafe { free(NonNull::new_unchecked(slots[s])) };
                }
                slots[s] = malloc(black_box(size)).expect("oom").as_ptr();
                i = i.wrapping_add(1);
            });
            for &p in &slots {
                if !p.is_null() {
                    // SAFETY: each non-null slot holds a live block.
                    unsafe { free(NonNull::new_unchecked(p)) };
                }
            }
        });
    }
    g.finish();
}

/// Mixed sizes with a bounded live set: a deterministic xorshift drives an
/// alloc/free mix across multiple bins (exercises bin selection + page churn).
fn mixed(c: &mut Criterion) {
    c.bench_function("mixed_sizes", |b| {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut live: Vec<*mut u8> = Vec::with_capacity(256);
        b.iter(|| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let r = x as usize;
            if live.len() >= 256 || (r & 1 == 0 && !live.is_empty()) {
                let idx = (r >> 1) % live.len();
                // SAFETY: `live` only holds blocks we allocated and not yet freed.
                unsafe { free(NonNull::new_unchecked(live.swap_remove(idx))) };
            } else {
                let size = 1 + (r >> 1) % 4096;
                live.push(malloc(black_box(size)).expect("oom").as_ptr());
            }
        });
        for p in live.drain(..) {
            // SAFETY: remaining live blocks, each freed once.
            unsafe { free(NonNull::new_unchecked(p)) };
        }
    });
}

criterion_group!(benches, alloc_free, ring, mixed);
criterion_main!(benches);
