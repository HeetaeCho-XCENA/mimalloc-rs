// SPDX-License-Identifier: MIT
//! Faithful port of mimalloc-bench `cache-thrash.cpp` (Hoard / Emery Berger):
//! each of N threads repeatedly allocates a small object, writes it
//! `repetitions` times, and frees it. A good allocator keeps each thread's
//! objects on distinct cache lines, so this should scale ~P-fold; a poor one
//! suffers passive false-sharing.
//!
//! Args: `cache_thrash [nthreads] [iterations] [objSize] [repetitions]`
//! (defaults P / 1000 / 1 / 1_000_000, matching the C `P 1000 1 1000000 P`).

use mimalloc_rs_bench::{arg, run_threads, timed};
use std::alloc::{alloc, dealloc, Layout};

fn main() {
    let p = std::thread::available_parallelism().map_or(8, |n| n.get());
    let nthreads = arg(1, p).max(1);
    let iterations = arg(2, 1000);
    let obj_size = arg(3, 1).max(1);
    let repetitions = arg(4, 1_000_000);
    let per_thread_reps = repetitions / nthreads;
    println!(
        "cache-thrash: {nthreads}T iterations={iterations} objSize={obj_size} repetitions={repetitions}"
    );
    let layout = Layout::from_size_align(obj_size, 1).unwrap();
    timed("cache-thrash", || {
        run_threads(nthreads, |_tid| {
            for _ in 0..iterations {
                // `new char[objSize]` (uninitialized), write it reps×size times, free.
                // SAFETY: non-zero layout; `obj` owns `obj_size` bytes; freed below.
                let obj = unsafe { alloc(layout) };
                assert!(!obj.is_null(), "oom");
                for _ in 0..per_thread_reps {
                    for k in 0..obj_size {
                        // SAFETY: k < obj_size; volatile to match the C write/read.
                        unsafe {
                            obj.add(k).write_volatile(k as u8);
                            let _ = obj.add(k).read_volatile();
                        }
                    }
                }
                // SAFETY: same layout the allocation used.
                unsafe { dealloc(obj, layout) };
            }
        });
    });
}
