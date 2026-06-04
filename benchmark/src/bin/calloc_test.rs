// SPDX-License-Identifier: MIT
//! Zeroed-allocation + realistic first-touch stress (no direct mimalloc-bench
//! counterpart; fills the `calloc`/`zalloc` gap the other workloads leave open —
//! none of them exercise the `free_is_zero` zeroing path).
//!
//! Each thread, for `iters` rounds: `calloc`s a `size`-byte buffer, **verifies
//! it is zero while touching one byte per OS page** (so the kernel actually
//! faults the pages — the cost a real consumer of zeroed memory pays, which a
//! never-touched allocation hides), writes a marker, and frees it. Run it at a
//! few sizes to cover small → huge.
//!
//! Args: `calloc_test [threads] [iters] [size_kib]`

use mimalloc_rs_bench::{arg, run_threads, timed};

fn main() {
    let threads = arg(1, 1).max(1);
    let iters = arg(2, 100_000);
    let size = arg(3, 64) * 1024;
    println!("calloc-test: {threads}T x {iters} iters, {size} B zeroed + touched");
    timed("calloc-test", || {
        run_threads(threads, |_| {
            let mut nonzero: u64 = 0;
            for _ in 0..iters {
                let mut v = vec![0u8; size];
                // Touch one byte per 4 KiB page: read (must be zero) then write.
                let mut i = 0;
                while i < size {
                    nonzero += (v[i] != 0) as u64;
                    v[i] = 1;
                    i += 4096;
                }
                std::hint::black_box(&v);
            }
            // `calloc` must hand back zeroed memory; fold the check so the reads
            // above can't be optimized away.
            assert_eq!(nonzero, 0, "calloc returned non-zero memory");
        });
    });
}
