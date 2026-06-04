// SPDX-License-Identifier: MIT
//! Faithful port of mimalloc-bench `malloc-large.cpp` (Leonid Stolyarov / Daan
//! Leijen): up to 20 live buffers of 5–25 MiB, replaced over 2000 iterations.
//! Exercises the large/huge allocation + commit/purge path. (The C `mt19937(42)`
//! is substituted with splitmix64 over the same 5–25 MiB uniform range.)

use mimalloc_rs_bench::{arg, pick, timed};

const NUM_BUFFERS: usize = 20;
const MIN: usize = 5 * 1024 * 1024;
const MAX: usize = 25 * 1024 * 1024;

fn main() {
    let iters = arg(1, 2000);
    println!(
        "malloc-large: {NUM_BUFFERS} live buffers, {}-{} MiB, {iters} iters",
        MIN >> 20,
        MAX >> 20
    );
    timed("malloc-large", || {
        let mut buffers: [Option<Vec<u8>>; NUM_BUFFERS] = std::array::from_fn(|_| None);
        let mut r: u64 = 42;
        for _ in 0..iters {
            let idx = (pick(&mut r) as usize) % NUM_BUFFERS;
            let size = MIN + (pick(&mut r) as usize) % (MAX - MIN + 1);
            // make_unique<char[]>(size) zero-initializes; this drops the old buffer.
            buffers[idx] = Some(vec![0u8; size]);
        }
        std::hint::black_box(&buffers);
    });
}
