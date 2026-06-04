// SPDX-License-Identifier: MIT
//! Faithful port of mimalloc-bench `larson` (Paul Larson, Microsoft):
//! `larson/larson.cpp` (the `_MT`/`_REENTRANT` `runthreads` + `exercise_heap` +
//! `warmup` path; the C version compiled by mimalloc-bench, i.e. malloc/free).
//!
//! A server-style allocation stress. `threads` threads each own a slot array of
//! `chunks` blocks carved out of one shared block table (C `blkp` partitioned as
//! `de_area[i].array = &blkp[i*chperthread]`). After a `warmup` (fill, random
//! permutation, then 4×chunks free+alloc churn), each thread repeatedly: picks a
//! random victim slot, frees the block there, and allocates a new block of
//! random size in `[min_size, max_size)` into it, writing two bytes. It runs for
//! a fixed duration (`secs`) and reports throughput as alloc ops per second.
//!
//! Cross-thread aspect: blocks live in the shared table and each thread mutates
//! its own contiguous partition; the C driver respawns finished threads onto the
//! same partition until `stopflag`. Here scoped threads loop on a shared
//! `stop` flag for the duration, which preserves the per-thread slot ownership
//! and total churn the C measures.
//!
//! PRNG substitution: the C `lran2` portable RNG (for seeds, victims, sizes, and
//! the warmup permutation) is replaced by the harness splitmix64 `pick` over the
//! same `% range` / `% chunks` ranges. Blocks use raw `std::alloc`
//! (C `malloc`/`free`).
//!
//! Args (positional, mirroring C `larson <secs> <min> <max> <chunks> <rounds> <seed> <threads>`):
//! `larson [secs] [min] [max] [chunks] [rounds] [seed] [threads]`
//! defaults `5 1000 5000 100 4141 0 8` (seed unused beyond per-thread offset).
//! Prints `BENCH larson [<alloc>] OPS_PER_SEC <n>`.

use mimalloc_rs_bench::{alloc_name, arg, pick};
use std::alloc::{alloc, dealloc, Layout};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

const ALIGN: usize = 16;

/// One slot: a live block plus its size (C `array[]` + `blksize[]`).
#[derive(Clone, Copy)]
struct Slot {
    ptr: *mut u8,
    sz: usize,
}

/// `Send` wrapper for handing a thread's slot partition into a scoped thread.
struct Partition<'a>(&'a mut [Slot]);
// SAFETY: each partition is a disjoint sub-slice of the block table given to
// exactly one thread; the raw `*mut u8` blocks inside are single-owner.
unsafe impl Send for Partition<'_> {}

#[inline]
fn alloc_block(sz: usize) -> *mut u8 {
    let layout = Layout::from_size_align(sz.max(1), ALIGN).unwrap();
    // SAFETY: non-zero layout; freed with `free_block(_, sz)`.
    let p = unsafe { alloc(layout) };
    assert!(!p.is_null(), "oom");
    p
}

#[inline]
fn free_block(ptr: *mut u8, sz: usize) {
    let layout = Layout::from_size_align(sz.max(1), ALIGN).unwrap();
    // SAFETY: `ptr` came from `alloc_block(sz)` with this exact layout.
    unsafe { dealloc(ptr, layout) };
}

/// C `exercise_heap` body for one slot partition, run for the duration.
/// Returns the number of alloc operations performed.
fn exercise_heap(
    slots: &mut [Slot],
    min_size: usize,
    range: usize,
    rounds: usize,
    seed: u64,
    stop: &AtomicBool,
) -> u64 {
    let asize = slots.len();
    let num_blocks = rounds * asize; // C `NumBlocks = num_rounds*nperthread`
    let mut r = seed;
    let mut allocs: u64 = 0;
    // C respawns the thread until stopflag; we loop the NumBlocks batch instead.
    'outer: loop {
        for _ in 0..num_blocks {
            let victim = (pick(&mut r) as usize) % asize;
            free_block(slots[victim].ptr, slots[victim].sz);
            let blk_size = if range == 0 {
                min_size
            } else {
                min_size + (pick(&mut r) as usize) % range
            };
            let p = alloc_block(blk_size);
            // C writes 'a' at [0] and 'b' at [1] (volatile).
            // SAFETY: `blk_size >= 1`; write [1] only when there are 2 bytes.
            unsafe {
                p.write_volatile(b'a');
                let _ = p.read_volatile();
                if blk_size > 1 {
                    p.add(1).write_volatile(b'b');
                }
            }
            slots[victim] = Slot { ptr: p, sz: blk_size };
            allocs += 1;
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
        }
    }
    allocs
}

/// C `warmup`: fill the partition, random-permute it, then 4×chunks free+alloc.
fn warmup(slots: &mut [Slot], min_size: usize, range: usize, r: &mut u64) {
    let n = slots.len();
    for slot in slots.iter_mut() {
        let blk_size = if range == 0 {
            min_size
        } else {
            min_size + (pick(r) as usize) % range
        };
        *slot = Slot {
            ptr: alloc_block(blk_size),
            sz: blk_size,
        };
    }
    // random permutation (C warmup permute loop)
    for cblks in (1..=n).rev() {
        let victim = (pick(r) as usize) % cblks;
        slots.swap(victim, cblks - 1);
    }
    // 4×n free+alloc churn
    for _ in 0..4 * n {
        let victim = (pick(r) as usize) % n;
        free_block(slots[victim].ptr, slots[victim].sz);
        let blk_size = if range == 0 {
            min_size
        } else {
            min_size + (pick(r) as usize) % range
        };
        slots[victim] = Slot {
            ptr: alloc_block(blk_size),
            sz: blk_size,
        };
    }
}

fn main() {
    let secs: f64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5.0);
    let min_size = arg(2, 1000);
    let max_size = arg(3, 5000).max(min_size + 1);
    let chperthread = arg(4, 100).max(1);
    let rounds = arg(5, 4141).max(1);
    let seed = arg(6, 0) as u64;
    let threads = arg(7, 8).max(1);
    let range = max_size - min_size;

    println!(
        "larson: {threads}T secs={secs} size=[{min_size},{max_size}) chunks/thread={chperthread} rounds={rounds}"
    );

    // Shared block table, partitioned per thread (C `blkp`).
    let total_chunks = chperthread * threads;
    let mut table = vec![Slot { ptr: std::ptr::null_mut(), sz: 0 }; total_chunks];

    // Warmup each partition single-threaded (C warms up before launching).
    {
        let mut r = seed.wrapping_add(0x9e3779b97f4a7c15);
        for part in table.chunks_mut(chperthread) {
            warmup(part, min_size, range, &mut r);
        }
    }

    let stop = AtomicBool::new(false);
    let total_allocs = AtomicU64::new(0);
    let start = Instant::now();

    std::thread::scope(|s| {
        for (tid, part) in table.chunks_mut(chperthread).enumerate() {
            let stop = &stop;
            let total_allocs = &total_allocs;
            let part = Partition(part);
            s.spawn(move || {
                let part = part;
                let tseed = seed
                    .wrapping_add(tid as u64 + 1)
                    .wrapping_mul(0xd989bcacc137dcd5);
                let a = exercise_heap(part.0, min_size, range, rounds, tseed, stop);
                total_allocs.fetch_add(a, Ordering::Relaxed);
            });
        }
        while start.elapsed().as_secs_f64() < secs {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        stop.store(true, Ordering::Relaxed);
    });

    // Free everything still live.
    for slot in &table {
        if !slot.ptr.is_null() {
            free_block(slot.ptr, slot.sz);
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let allocs = total_allocs.load(Ordering::Relaxed);
    let ops_per_sec = allocs as f64 / elapsed;
    println!(
        "larson: {allocs} ops in {elapsed:.3}s (throughput {ops_per_sec:.0} ops/sec)"
    );
    eprintln!(
        "BENCH larson [{}] OPS_PER_SEC {:.0}",
        alloc_name(),
        ops_per_sec
    );
}
