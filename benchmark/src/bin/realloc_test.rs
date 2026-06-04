// SPDX-License-Identifier: MIT
//! Reallocation churn (no direct mimalloc-bench counterpart; fills the `realloc`
//! gap — the other workloads only malloc/free). Exercises the grow path (which
//! must move + copy when the block can't extend in place) and freeing.
//!
//! Each thread keeps `slots` independently-growing buffers. Every round it picks
//! a slot and either grows it by a random amount (a `realloc` that copies the
//! live prefix and zero-extends the tail) or, once it passes a cap, frees it and
//! starts over — a mix of in-place and moving reallocs across size classes,
//! always touching the new tail.
//!
//! Args: `realloc_test [threads] [iters] [slots]`

use mimalloc_rs_bench::{arg, pick, run_threads, timed};

const CAP: usize = 64 * 1024;

fn main() {
    let threads = arg(1, 1).max(1);
    let iters = arg(2, 3_000_000);
    let slots = arg(3, 256).max(1);
    println!("realloc-test: {threads}T x {iters} iters, {slots} slots, cap {CAP} B");
    timed("realloc-test", || {
        run_threads(threads, |tid| {
            let mut r = (tid as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let mut bufs: Vec<Vec<u8>> = (0..slots).map(|_| Vec::new()).collect();
            let mut acc: u64 = 0;
            for _ in 0..iters {
                let s = (pick(&mut r) as usize) % slots;
                let cur = bufs[s].len();
                if cur >= CAP {
                    bufs[s] = Vec::new(); // free
                } else {
                    let add = 1 + (pick(&mut r) as usize) % 4096;
                    bufs[s].resize(cur + add, 0); // realloc-grow (copies `cur`, zeroes `add`)
                    let last = bufs[s].len() - 1;
                    bufs[s][last] = bufs[s][last].wrapping_add(1); // touch the new tail
                    acc = acc.wrapping_add(bufs[s][0] as u64);
                }
            }
            std::hint::black_box((&bufs, acc));
        });
    });
}
