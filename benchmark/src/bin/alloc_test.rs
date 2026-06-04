// SPDX-License-Identifier: MIT
//! Faithful port of mimalloc-bench `alloc-test` (OLogN Technologies AG):
//! `alloc-test/allocator_tester.h` (the `randomPos_RandomSize<…, MEM_ACCESS_TYPE::full>`
//! template + size distribution) and `alloc-test/allocator_tester.cpp` (the
//! `runRandomTest` / `runTest` thread driver, `main` defaults).
//!
//! Each thread keeps a working set of `maxItems` slots. A *setup* phase
//! saturates ~half the slots (1 bit in 2 of a random word) with freshly
//! allocated, fully-written blocks; the *main loop* then does `iterCount`
//! Pareto-80/20-indexed touch-or-replace operations: if the chosen slot holds a
//! block it is read (summed) and freed, otherwise a new random-sized block is
//! allocated and `memset` (MEM_ACCESS_TYPE::full). Finally every slot is freed.
//! Block sizes come from `calcSizeWithStatsAdjustment` (ctz-based size class,
//! maxItemSizeExp = 10 → up to ~1 KiB), giving the C size distribution exactly.
//!
//! PRNG substitution: the C `PRNG` (Arvid Gerstmann xorshift) is replaced with
//! the harness splitmix64 `pick`, fed through the *same* `calcSizeWithStatsAdjustment`
//! and `Pareto_80_20_6` math over the same ranges, so the size/index
//! distributions match. Blocks use raw `std::alloc` (the C `allocate`/`deallocate`
//! map to `malloc`/`free`).
//!
//! Args: `alloc_test [threads] [iters]` (default 6 threads, matching the C
//! `threadMax`). Total object budget is `1<<18` split across threads
//! (C `maxItems / threadCount`). The C `iterCount` is 100M ops/thread; that runs
//! for minutes, so the default here is 4M (deviation: a shorter fixed iteration
//! count for a tractable wall-clock) — pass a 2nd arg for the full count.

use mimalloc_rs_bench::{arg, pick, run_threads, timed};
use std::alloc::{alloc, dealloc, Layout};
use std::sync::atomic::{AtomicU64, Ordering};

const TOTAL_ITEMS: usize = 1 << 18; // 512k objects, C `maxItems`
const DEFAULT_ITER_COUNT: usize = 4_000_000; // shorter than C's 100M (see header)
const MAX_ITEM_SIZE_EXP: u32 = 10; // C `maxItemSize` (1k)
const ALIGN: usize = 16;

/// C `calcSizeWithStatsAdjustment`: a ctz-derived size class in `[1, ~2^exp]`.
#[inline]
fn calc_size_with_stats_adjustment(mut rand_num: u64, max_size_exp: u32) -> usize {
    debug_assert!(max_size_exp >= 3);
    let max_size_exp = max_size_exp - 3;
    // +1 to avoid a zero `stat_class_base` (ctz(0) is undefined in C).
    let stat_class_base = (rand_num & ((1u64 << max_size_exp) - 1)) + 1;
    rand_num >>= max_size_exp;
    let mut idx = stat_class_base.trailing_zeros();
    debug_assert!(idx <= max_size_exp);
    idx += 2;
    let sz_mask = (1u64 << idx) - 1;
    ((rand_num & sz_mask) + 1 + (1u64 << idx)) as usize
}

/// C `Pareto_80_20_6` constants (probability weights for the 7 index buckets).
const PARETO_80_20_6: [f64; 7] = [
    0.262144000000,
    0.393216000000,
    0.245760000000,
    0.081920000000,
    0.015360000000,
    0.001536000000,
    0.000064000000,
];

/// Precomputed C `Pareto_80_20_6_Data`.
struct Pareto {
    probability_ranges: [u32; 6],
    offsets: [u32; 8],
}

impl Pareto {
    fn new(item_count: u32) -> Self {
        let mut probability_ranges = [0u32; 6];
        probability_ranges[0] = (u32::MAX as f64 * PARETO_80_20_6[0]) as u32;
        probability_ranges[5] = (u32::MAX as f64 * (1.0 - PARETO_80_20_6[6])) as u32;
        for i in 1..5 {
            probability_ranges[i] =
                probability_ranges[i - 1] + (u32::MAX as f64 * PARETO_80_20_6[i]) as u32;
        }
        let mut offsets = [0u32; 8];
        offsets[7] = item_count;
        for i in 0..6 {
            offsets[i + 1] = offsets[i] + (item_count as f64 * PARETO_80_20_6[6 - i]) as u32;
        }
        Pareto {
            probability_ranges,
            offsets,
        }
    }

    /// C `Pareto_80_20_6_Rand`: map two randoms to a slot index.
    #[inline]
    fn rand(&self, rnum1: u32, rnum2: u32) -> usize {
        let mut idx = 6usize;
        for (i, &range) in self.probability_ranges.iter().enumerate() {
            if rnum1 < range {
                idx = i;
                break;
            }
        }
        let range_size = self.offsets[idx + 1] - self.offsets[idx];
        let offset_in_range = rnum2 % range_size;
        (self.offsets[idx] + offset_in_range) as usize
    }
}

/// One working-set slot (C `TestBin`, minus the `check`-mode reincarnation).
#[derive(Clone, Copy)]
struct Bin {
    ptr: *mut u8,
    sz: u32,
}

fn main() {
    let threads = arg(1, 6).max(1);
    let max_items = (TOTAL_ITEMS / threads).max(64);
    let iter_count = arg(2, DEFAULT_ITER_COUNT);
    println!(
        "alloc-test: {threads}T maxItems={max_items} iterCount={iter_count} maxItemSizeExp={MAX_ITEM_SIZE_EXP} mem-access=full"
    );

    let ops = AtomicU64::new(0);
    timed("alloc-test", || {
        run_threads(threads, |tid| {
            let mut r: u64 = 41 + tid as u64; // C `rndSeed = 41`, per-thread offset
            let pareto = Pareto::new(max_items as u32);
            let mut bins = vec![Bin { ptr: std::ptr::null_mut(), sz: 0 }; max_items];

            // setup (saturation): ~half the slots get a fresh, fully-written block.
            for i in 0..max_items / 32 {
                let rand_word = pick(&mut r) as u32;
                for j in 0..32 {
                    if (rand_word >> j) & 1 == 1 {
                        let sz = calc_size_with_stats_adjustment(pick(&mut r), MAX_ITEM_SIZE_EXP);
                        let idx = i * 32 + j;
                        let p = alloc_block(sz);
                        // SAFETY: `p` owns `sz` bytes (full MEM_ACCESS write).
                        unsafe { std::ptr::write_bytes(p, sz as u8, sz) };
                        bins[idx] = Bin { ptr: p, sz: sz as u32 };
                    }
                }
            }

            // main loop: C runs 32 outer × (iterCount>>5) inner = iterCount ops.
            let mut local_ops: u64 = 0;
            for _k in 0..32 {
                for _j in 0..iter_count >> 5 {
                    let rnum1 = pick(&mut r) as u32;
                    let rnum2 = pick(&mut r) as u32;
                    let idx = pareto.rand(rnum1, rnum2);
                    if !bins[idx].ptr.is_null() {
                        // touch (sum bytes), then free.
                        let sz = bins[idx].sz as usize;
                        let mut acc = 0usize;
                        // SAFETY: `ptr` holds `sz` valid bytes.
                        unsafe {
                            for i in 0..sz {
                                acc += *bins[idx].ptr.add(i) as usize;
                            }
                        }
                        std::hint::black_box(acc);
                        free_block(bins[idx].ptr, sz);
                        bins[idx].ptr = std::ptr::null_mut();
                    } else {
                        let sz =
                            calc_size_with_stats_adjustment(pick(&mut r), MAX_ITEM_SIZE_EXP);
                        let p = alloc_block(sz);
                        // SAFETY: `p` owns `sz` bytes (full MEM_ACCESS write).
                        unsafe { std::ptr::write_bytes(p, sz as u8, sz) };
                        bins[idx] = Bin { ptr: p, sz: sz as u32 };
                    }
                    local_ops += 1;
                }
            }

            // exit: free everything still live.
            for bin in bins.iter() {
                if !bin.ptr.is_null() {
                    free_block(bin.ptr, bin.sz as usize);
                }
            }
            ops.fetch_add(local_ops, Ordering::Relaxed);
        });
    });
    println!(
        "alloc-test: total operations = {}",
        ops.load(Ordering::Relaxed)
    );
}

/// `malloc(sz)` → raw alloc with a fixed 16-byte alignment.
#[inline]
fn alloc_block(sz: usize) -> *mut u8 {
    let layout = Layout::from_size_align(sz.max(1), ALIGN).unwrap();
    // SAFETY: non-zero size; caller frees with `free_block` using the same size.
    let p = unsafe { alloc(layout) };
    assert!(!p.is_null(), "oom");
    p
}

/// `free(ptr)` for a block allocated by `alloc_block(sz)`.
#[inline]
fn free_block(ptr: *mut u8, sz: usize) {
    let layout = Layout::from_size_align(sz.max(1), ALIGN).unwrap();
    // SAFETY: `ptr` came from `alloc_block(sz)` with this exact layout.
    unsafe { dealloc(ptr, layout) };
}
