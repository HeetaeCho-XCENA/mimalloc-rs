// SPDX-License-Identifier: MIT
//! A small allocator micro-benchmark: mimalloc-rs vs the system allocator on a
//! mixed alloc/free workload with a live window. Not a rigorous benchmark — a
//! quick relative signal. Run with: `cargo run --release --example bench`.

use mimalloc_rs::MiMalloc;
use std::alloc::{GlobalAlloc, Layout, System};
use std::time::Instant;

/// Mixed-size alloc/free workload keeping ~`window` live allocations.
fn workload<A: GlobalAlloc>(a: &A, rounds: usize, window: usize) -> u64 {
    let mut live: Vec<(*mut u8, Layout)> = Vec::with_capacity(window + 1);
    let mut x: u64 = 0x1234_5678_9abc_def1;
    let mut checksum: u64 = 0;
    for _ in 0..rounds {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let sz = 8 + (x as usize % 1024);
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
    checksum
}

fn bench<A: GlobalAlloc>(name: &str, a: &A, rounds: usize) {
    // warm up
    let _ = workload(a, rounds / 10, 2048);
    let t = Instant::now();
    let cs = workload(a, rounds, 2048);
    let dt = t.elapsed();
    let mops = rounds as f64 / dt.as_secs_f64() / 1e6;
    println!("{name:<14} {rounds} ops in {dt:>10.2?}  =>  {mops:7.1} Mops/s   (cs={cs:#x})");
}

fn main() {
    let rounds = 5_000_000;
    println!("mixed alloc/free, size 8..1032, 2048 live, {rounds} rounds\n");
    bench("system", &System, rounds);
    bench("mimalloc-rs", &MiMalloc, rounds);
}
