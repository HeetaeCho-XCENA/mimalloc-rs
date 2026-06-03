// SPDX-License-Identifier: MIT
//! Allocator stress benchmark — a faithful Rust port of mimalloc's official
//! `test/test-stress.c` (Copyright (c) 2018-2025 Microsoft Research, Daan
//! Leijen; MIT), driven through Rust's global allocator so mimalloc-rs runs at
//! its **peak** (statically linked, fully inlined — its best-case environment,
//! unlike the `LD_PRELOAD` cdylib which crosses an export boundary).
//!
//! It mirrors the C workload exactly: a deterministic `splitmix64` PRNG, the
//! same `pick`/`chance` distribution, `alloc_items` with the cookie fill +
//! verify-on-free, a shared cross-thread transfer buffer, retained objects, and
//! `ITER` rounds of thread creation/destruction. Rolling per-allocation heaps
//! (`MI_USE_HEAPS`) are intentionally not used, so both this and a same-built C
//! `test-stress` exercise the default per-thread heap — an apples-to-apples
//! comparison of the two allocators at their best.
//!
//! It is **not** a micro-benchmark of one path; like the original it reflects a
//! mixed real-world workload and depends on (deterministic) thread scheduling.
//!
//! ## Usage
//! ```text
//! cargo build --release --example stress
//! ./target/release/examples/stress [THREADS] [SCALE] [ITER] [NUMA_NODE]
//! ```
//! * `THREADS` — worker threads per round (default 32).
//! * `SCALE` — load factor per thread (default 50); `> 100` enables very large objects.
//! * `ITER` — rounds of destroy/recreate-all-threads (default 50).
//! * `NUMA_NODE` — optional (Linux): bind memory (`set_mempolicy(MPOL_BIND)`) and thread CPU affinity to this NUMA node.
//!
//! Elapsed seconds for the timed region are printed to stderr as
//! `STRESS_SECONDS <n>` (so a harness can `grep` it); pin with `taskset` for
//! repeatable numbers, e.g. `taskset -c 2-9 ./stress 8 50 50`.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Instant;

use mimalloc_rs::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const COOKIE: usize = 0xbf58476d1ce4e5b9;
const TRANSFERS: usize = 1000;
const WORD: usize = core::mem::size_of::<usize>();

/// Shared cross-thread transfer buffer (C's `volatile void* transfer[1000]`).
struct Transfer([AtomicPtr<usize>; TRANSFERS]);
// SAFETY: every access is through the atomics; the array is process-global.
unsafe impl Sync for Transfer {}
static TRANSFER: Transfer = Transfer([const { AtomicPtr::new(core::ptr::null_mut()) }; TRANSFERS]);

/// Run configuration, fixed after argument parsing and passed by value to each
/// worker (no mutable globals).
#[derive(Clone, Copy)]
struct Cfg {
    threads: usize,
    scale: usize,
    iter: usize,
    allow_large: bool,
}

#[inline]
fn pick(r: &mut u64) -> u64 {
    // splitmix64 (the 64-bit branch of test-stress.c's `pick`).
    let mut x = *r;
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^= x >> 31;
    *r = x;
    x
}

#[inline]
fn chance(perc: u64, r: &mut u64) -> bool {
    pick(r) % 100 <= perc
}

#[inline]
fn layout(items: usize) -> Layout {
    Layout::from_size_align(items * WORD, WORD).unwrap()
}

/// Allocate `items` (after scaling) zeroed words and fill the cookie pattern.
/// The scaled item count is recovered at free from word 0, so no side table of
/// sizes is needed. Returns null only on allocation failure.
fn alloc_items(mut items: usize, allow_large: bool, r: &mut u64) -> *mut usize {
    if chance(1, r) {
        if chance(1, r) && allow_large {
            items *= 10000; // 0.01% giant
        } else if chance(10, r) && allow_large {
            items *= 1000; // 0.1% huge
        } else {
            items *= 100; // 1% large
        }
    }
    if (32..=40).contains(&items) {
        items *= 2; // mirror the original's pthreads-320b note
    }
    if items == 0 {
        items = 1;
    }
    // SAFETY: non-zero layout; `alloc_zeroed` yields `items` zeroed words.
    let p = unsafe { alloc_zeroed(layout(items)) as *mut usize };
    if !p.is_null() {
        for i in 0..items {
            // SAFETY: `i < items`, `p` owns `items` words.
            unsafe {
                debug_assert_eq!(*p.add(i), 0);
                *p.add(i) = (items - i) ^ COOKIE;
            }
        }
    }
    p
}

/// Verify the cookie pattern (detecting corruption) and free. `p` may be null.
fn free_items(p: *mut usize) {
    if p.is_null() {
        return;
    }
    // SAFETY: `p` came from `alloc_items`; word 0 holds `items ^ COOKIE`.
    let items = unsafe { *p } ^ COOKIE;
    for i in 0..items {
        // SAFETY: `i < items`.
        if (unsafe { *p.add(i) } ^ COOKIE) != items - i {
            eprintln!("memory corruption at block {p:p} at {i}");
            std::process::abort();
        }
    }
    // SAFETY: same layout the allocation used (recovered `items`).
    unsafe { dealloc(p as *mut u8, layout(items)) };
}

#[inline]
fn transfer_xchg(idx: usize, newval: *mut usize) -> *mut usize {
    TRANSFER.0[idx].swap(newval, Ordering::SeqCst)
}

fn stress(tid: usize, cfg: Cfg) {
    let mut r: u64 = ((tid as u64) + 1) * 43;
    let max_item_shift: u64 = 5; // 1..16 words (8..128 bytes)
    let max_item_retained_shift: u64 = max_item_shift + 2; // 1..64 words
    let mut allocs: usize = 100 * cfg.scale * (tid % 8 + 1); // some threads do more
    let mut retain: usize = allocs / 2;

    let mut data: Vec<*mut usize> = Vec::new(); // grows; holds NULL holes
    let mut retained: Vec<*mut usize> = Vec::with_capacity(retain);

    while allocs > 0 || retain > 0 {
        if retain == 0 || (chance(50, &mut r) && allocs > 0) {
            // 50%+ allocate and keep in the working set
            allocs -= 1;
            let items = 1usize << (pick(&mut r) % max_item_shift);
            data.push(alloc_items(items, cfg.allow_large, &mut r));
        } else {
            // 25% allocate and retain to the end
            let items = 1usize << (pick(&mut r) % max_item_retained_shift);
            retained.push(alloc_items(items, cfg.allow_large, &mut r));
            retain -= 1;
        }
        if chance(66, &mut r) && !data.is_empty() {
            // 66% free a previous allocation
            let idx = (pick(&mut r) as usize) % data.len();
            free_items(data[idx]);
            data[idx] = core::ptr::null_mut();
        }
        if chance(25, &mut r) && !data.is_empty() {
            // 25% exchange a local pointer with the shared transfer buffer
            let data_idx = (pick(&mut r) as usize) % data.len();
            let transfer_idx = (pick(&mut r) as usize) % TRANSFERS;
            data[data_idx] = transfer_xchg(transfer_idx, data[data_idx]);
        }
    }

    for &p in &retained {
        free_items(p);
    }
    for &p in &data {
        free_items(p);
    }
}

fn run_round(cfg: Cfg) {
    let handles: Vec<_> = (0..cfg.threads)
        .map(|i| std::thread::spawn(move || stress(i, cfg)))
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

fn test_stress(cfg: Cfg) {
    let mut r: u64 = 0x7feb352d; // deterministic seed (C: srand(0x7feb352d))
    for n in 0..cfg.iter {
        run_round(cfg);
        // Free half the transfer buffer between rounds (all of it on the last).
        for i in 0..TRANSFERS {
            if chance(50, &mut r) || n + 1 == cfg.iter {
                free_items(transfer_xchg(i, core::ptr::null_mut()));
            }
        }
    }
    // Final drain of anything still parked in the transfer buffer.
    for i in 0..TRANSFERS {
        free_items(transfer_xchg(i, core::ptr::null_mut()));
    }
}

/// Bind memory allocation and thread CPU affinity to a NUMA node (Linux). Set
/// once on the main thread *before* spawning workers: both the memory policy
/// and the affinity mask are inherited by threads created afterwards. Best
/// effort — warns (does not abort) if the platform/kernel refuses.
#[cfg(target_os = "linux")]
fn bind_numa_node(node: usize) {
    // Memory: set_mempolicy(MPOL_BIND, {node}). MPOL_BIND == 2.
    let mask: u64 = 1u64 << node;
    // `maxnode` counts bits scanned in the mask; one word (64 nodes) is plenty.
    // SAFETY: passing a valid pointer to a 64-bit mask and its bit length.
    let rc = unsafe { libc::syscall(libc::SYS_set_mempolicy, 2_i64, &mask as *const u64, 64_i64) };
    if rc != 0 {
        eprintln!(
            "stress: warning: set_mempolicy(node {node}) failed (errno {})",
            unsafe { *libc::__errno_location() }
        );
    }
    // CPU affinity: pin to the node's CPUs (from /sys node cpulist).
    match node_cpus(node) {
        Some(cpus) if !cpus.is_empty() => {
            // SAFETY: zeroed cpu_set_t, then set the node's CPUs and apply it.
            unsafe {
                let mut set: libc::cpu_set_t = core::mem::zeroed();
                libc::CPU_ZERO(&mut set);
                for c in &cpus {
                    libc::CPU_SET(*c, &mut set);
                }
                if libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
                    eprintln!("stress: warning: sched_setaffinity(node {node}) failed");
                }
            }
            eprintln!(
                "stress: bound to NUMA node {node} ({} CPUs, memory MPOL_BIND)",
                cpus.len()
            );
        }
        _ => eprintln!("stress: warning: no CPU list for NUMA node {node}; memory bound only"),
    }
}

/// Parse `/sys/devices/system/node/node{N}/cpulist` (e.g. `0-7,16-23`).
#[cfg(target_os = "linux")]
fn node_cpus(node: usize) -> Option<Vec<usize>> {
    let path = format!("/sys/devices/system/node/node{node}/cpulist");
    let text = std::fs::read_to_string(path).ok()?;
    let mut cpus = Vec::new();
    for part in text.trim().split(',') {
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((a, b)) => {
                let a: usize = a.trim().parse().ok()?;
                let b: usize = b.trim().parse().ok()?;
                cpus.extend(a..=b);
            }
            None => cpus.push(part.trim().parse::<usize>().ok()?),
        }
    }
    Some(cpus)
}

#[cfg(not(target_os = "linux"))]
fn bind_numa_node(_node: usize) {
    eprintln!("stress: NUMA binding is only implemented on Linux; ignoring");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize| {
        args.get(i)
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
    };

    let threads = arg(1).unwrap_or(32);
    let scale = arg(2).unwrap_or(50);
    let iter = arg(3).unwrap_or(50);
    let numa = args.get(4).and_then(|s| s.parse::<usize>().ok());
    let cfg = Cfg {
        threads,
        scale,
        iter,
        allow_large: scale > 100,
    };

    if let Some(node) = numa {
        bind_numa_node(node);
    }

    println!(
        "Using {} threads with a {}% load-per-thread and {} iterations{}",
        cfg.threads,
        cfg.scale,
        cfg.iter,
        if cfg.allow_large {
            " (allow large objects)"
        } else {
            ""
        }
    );

    let t = Instant::now();
    test_stress(cfg);
    eprintln!("STRESS_SECONDS {:.4}", t.elapsed().as_secs_f64());
}
