// SPDX-License-Identifier: MIT
//! A more rigorous allocator micro-benchmark. Compares the system allocator,
//! the C reference `libmimalloc` (optional), and mimalloc-rs across several
//! workload shapes:
//!
//!   1. Repetition — each timed pass runs `REPS` times; we report **min**
//!      (cleanest signal, least interference) and **median** (typical case)
//!      instead of a single noisy sample.
//!   2. Interleaved order — within every rep we run the allocators in turn, so
//!      none permanently gets the "CPU already at full turbo" advantage of
//!      always running second.
//!   3. Core pinning — the single-threaded phases pin the process to one core
//!      via `sched_setaffinity` (Linux), removing P/E-core migration jitter on
//!      hybrid CPUs (e.g. i7-14700K). Override the core with `BENCH_PIN_CORE`.
//!      For belt-and-suspenders, also run under `taskset -c <core>`.
//!   4. Multi-threaded phase — a `threadtest`-style scaling run across several
//!      thread counts, which is where mimalloc's per-thread heaps pay off.
//!   5. Size coverage — beyond the small fast path (8..1032), separate phases
//!      exercise the large-page path (16K..512K) and the huge path (>512K,
//!      1..4 MiB), which take entirely different code paths in the allocator.
//!   6. Cross-thread free — a producer/consumer phase where one thread
//!      allocates and a *different* thread frees, exercising mimalloc's atomic
//!      thread-free list (the intra-thread phases never hit that path).
//!
//! Two-way run (system vs mimalloc-rs):
//!   `cargo run --release --example bench_suite`
//! Three-way run (adds the C reference allocator):
//!   `MIMALLOC_C_LIB=/path/to/mimalloc/build cargo run --release --example bench_suite`
//!
//! IMPORTANT: the C library is loaded at run time via `dlopen` with
//! `RTLD_LOCAL`, deliberately NOT linked. A normally-linked `libmimalloc.so`
//! exports `malloc`/`free` as interposing symbols, so the dynamic linker would
//! rebind the *process's* `malloc` (and thus `System`) to mimalloc — silently
//! turning the "system" baseline into a second mimalloc. `RTLD_LOCAL` keeps
//! mimalloc's symbols out of the global scope, so `System` stays glibc.
//!
//! Still a micro-benchmark — a relative signal, not a real application.
//!
//! ## Sample results (i7-14700K, Linux, pinned to core 2; 2026-06)
//!
//! Note: the C reference is called via an indirect `dlopen` function pointer
//! (no cross-language inlining), so its numbers carry a small per-call FFI
//! handicap that a natively-linked C program would not — it understates C on
//! the cheap small path and is negligible on the expensive large/huge paths.
//!
//! ```text
//! phase 1: single-threaded small (8..1032)   system 69.6 | mimalloc-c 106.1 | mimalloc-rs 76.2  Mops/s
//! phase 3a: single-threaded large (16K..512K) system  4.3 | mimalloc-c  19.5 | mimalloc-rs 32.8  Mops/s
//! phase 3b: single-threaded huge (1M..4M)     system  1.7 | mimalloc-c   3.1 | mimalloc-rs  4.2  Mops/s
//! phase 2: threadtest (intra-thread free), aggregate Mops/s
//!     threads        1       2       4       8
//!     system      68.1   135.8   243.1   411.0
//!     mimalloc-c  97.8   164.2   299.9   475.7
//!     mimalloc-rs 71.2   139.6   263.3   451.3      (scales 1->8: 6.3x vs C 4.9x)
//! phase 4: cross-thread free (alloc->free handoffs), aggregate Mops/s
//!     pairs          1       2       4
//!     system       4.7     9.6    18.0
//!     mimalloc-c  21.9    33.2    57.8
//!     mimalloc-rs 27.6    46.8   102.8
//! ```
//!
//! Takeaway: competitive-to-faster than the C reference on large/huge/
//! cross-thread and multi-thread scaling; the small-object single-threaded hot
//! path is the remaining gap (~40% behind C) and the current optimization
//! target.

use mimalloc_rs::MiMalloc;
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

/// The C reference allocator (`libmimalloc`), resolved at run time via `dlopen`.
/// Holds function pointers to the C entry points; not zero-sized, but `Copy`
/// and trivially `Send`/`Sync` (bare `fn` pointers are both).
#[derive(Clone, Copy)]
struct CMiMalloc {
    malloc_aligned: unsafe extern "C" fn(usize, usize) -> *mut u8,
    free: unsafe extern "C" fn(*mut u8),
}

// SAFETY: the two function pointers are the canonical C `mi_*` entry points
// resolved from libmimalloc; they honor the requested size+alignment (or return
// null) and free blocks they allocated, upholding the `GlobalAlloc` contract.
unsafe impl GlobalAlloc for CMiMalloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: FFI call with size>=1 and a power-of-two alignment.
        unsafe { (self.malloc_aligned)(layout.size().max(1), layout.align()) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: `ptr` came from `self.malloc_aligned` above.
        unsafe { (self.free)(ptr) }
    }
}

/// Try to load libmimalloc from `$MIMALLOC_C_LIB` (a directory) at run time.
/// Uses `RTLD_LOCAL` so mimalloc's `malloc`/`free` overrides do NOT interpose
/// the process allocator — `System` must remain glibc for a fair baseline.
#[cfg(unix)]
fn load_c_mimalloc() -> Option<CMiMalloc> {
    let dir = std::env::var("MIMALLOC_C_LIB").ok()?;
    if dir.is_empty() {
        return None;
    }
    let path = std::ffi::CString::new(format!("{dir}/libmimalloc.so")).ok()?;
    // SAFETY: standard dlopen/dlsym usage; we transmute the resolved symbols to
    // their known C signatures only after null-checking them.
    unsafe {
        let h = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if h.is_null() {
            eprintln!("warning: dlopen({dir}/libmimalloc.so) failed; running 2-way");
            return None;
        }
        let m = libc::dlsym(h, c"mi_malloc_aligned".as_ptr());
        let f = libc::dlsym(h, c"mi_free".as_ptr());
        if m.is_null() || f.is_null() {
            eprintln!("warning: libmimalloc missing mi_malloc_aligned/mi_free; running 2-way");
            return None;
        }
        Some(CMiMalloc {
            malloc_aligned: core::mem::transmute::<
                *mut core::ffi::c_void,
                unsafe extern "C" fn(usize, usize) -> *mut u8,
            >(m),
            free: core::mem::transmute::<*mut core::ffi::c_void, unsafe extern "C" fn(*mut u8)>(f),
        })
    }
}

#[cfg(not(unix))]
fn load_c_mimalloc() -> Option<CMiMalloc> {
    None
}

// ---- single-threaded small (phase 1) ----
const REPS: usize = 11;
const ROUNDS: usize = 5_000_000;
const WINDOW: usize = 2048;
const SMALL_MIN: usize = 8;
const SMALL_SPAN: usize = 1024; // sizes 8..1031 → small fast path (<= 1024)

// ---- multi-threaded small (phase 2) ----
const THREAD_COUNTS: &[usize] = &[1, 2, 4, 8];
const MT_ROUNDS_PER_THREAD: usize = 2_000_000;

// ---- large / huge single-threaded (phase 3) ----
// large: 16 KiB .. 512 KiB → medium + large pages (bin < MI_BIN_HUGE).
const LARGE_MIN: usize = 16 * 1024;
const LARGE_SPAN: usize = 512 * 1024 - 16 * 1024;
const LARGE_WINDOW: usize = 256;
const LARGE_ROUNDS: usize = 200_000;
// huge: 1 MiB .. 4 MiB → huge path (> 512 KiB; typically a dedicated mmap each).
const HUGE_MIN: usize = 1024 * 1024;
const HUGE_SPAN: usize = 3 * 1024 * 1024;
const HUGE_WINDOW: usize = 32;
const HUGE_ROUNDS: usize = 50_000;
const BIG_REPS: usize = 5;

// ---- cross-thread free (phase 4) ----
const XT_PAIRS: &[usize] = &[1, 2, 4];
const XT_ROUNDS_PER_PAIR: usize = 2_000_000;
const XT_CHAN_CAP: usize = 2048; // bounded → backpressure caps live memory
const XT_REPS: usize = 5;

/// Mixed-size alloc/free workload keeping ~`window` live allocations. `seed`
/// makes the (deterministic) size/free sequence distinct per thread while
/// staying identical across allocators, so comparisons remain apples-to-apples.
/// Sizes range over `size_min .. size_min + size_span`.
fn workload<A: GlobalAlloc>(
    a: &A,
    rounds: usize,
    window: usize,
    seed: u64,
    size_min: usize,
    size_span: usize,
) -> u64 {
    let mut live: Vec<(*mut u8, Layout)> = Vec::with_capacity(window + 1);
    let mut x: u64 = seed;
    let mut checksum: u64 = 0;
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
                *p = 1; // touch (forces at least one page fault)
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

/// One timed single-threaded pass (no warmup inside; warm up separately).
fn timed_pass<A: GlobalAlloc>(
    a: &A,
    rounds: usize,
    window: usize,
    size_min: usize,
    size_span: usize,
) -> Duration {
    let t = Instant::now();
    let cs = workload(
        a,
        rounds,
        window,
        0x1234_5678_9abc_def1,
        size_min,
        size_span,
    );
    let dt = t.elapsed();
    black_box(cs);
    dt
}

/// min and median of a set of samples (sorted copy; sample counts are small).
fn min_median(samples: &[Duration]) -> (Duration, Duration) {
    let mut v = samples.to_vec();
    v.sort();
    (v[0], v[v.len() / 2])
}

fn mops(ops: usize, dt: Duration) -> f64 {
    ops as f64 / dt.as_secs_f64() / 1e6
}

fn report(name: &str, ops: usize, samples: &[Duration]) {
    let (min, med) = min_median(samples);
    println!(
        "{name:<14} min {min:>9.2?} ({:6.1} Mops/s)   median {med:>9.2?} ({:6.1} Mops/s)",
        mops(ops, min),
        mops(ops, med),
    );
}

/// Interleaved single-threaded comparison over `reps` passes for a size range.
/// Runs the C reference too when `c` is `Some`.
fn run_single(
    c: Option<&CMiMalloc>,
    size_min: usize,
    size_span: usize,
    window: usize,
    rounds: usize,
    reps: usize,
) {
    // Warm up each (covers mimalloc lazy thread-heap init / first segment map).
    let _ = timed_pass(&System, rounds / 10, window, size_min, size_span);
    if let Some(c) = c {
        let _ = timed_pass(c, rounds / 10, window, size_min, size_span);
    }
    let _ = timed_pass(&MiMalloc, rounds / 10, window, size_min, size_span);

    let mut sys = Vec::with_capacity(reps);
    let mut cm = Vec::with_capacity(reps);
    let mut mi = Vec::with_capacity(reps);
    for _ in 0..reps {
        // Interleave so none always runs on the hotter CPU.
        sys.push(timed_pass(&System, rounds, window, size_min, size_span));
        if let Some(c) = c {
            cm.push(timed_pass(c, rounds, window, size_min, size_span));
        }
        mi.push(timed_pass(&MiMalloc, rounds, window, size_min, size_span));
    }
    report("system", rounds, &sys);
    if !cm.is_empty() {
        report("mimalloc-c", rounds, &cm);
    }
    report("mimalloc-rs", rounds, &mi);
}

/// Pin the calling thread to a single CPU core to remove scheduler/core-migration
/// jitter (notably P↔E core hops on hybrid CPUs). Returns whether it succeeded.
#[cfg(target_os = "linux")]
fn pin_to_core(core: usize) -> bool {
    // SAFETY: zero-initialized cpu_set_t is valid; CPU_SET sets one bit in range;
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

/// Restore affinity to all online cores (undo Phase 1 pinning before MT phases).
#[cfg(target_os = "linux")]
fn pin_to_all_cores() -> bool {
    // SAFETY: query online CPU count, build a full mask, apply to current thread.
    unsafe {
        let n = libc::sysconf(libc::_SC_NPROCESSORS_ONLN);
        if n <= 0 {
            return false;
        }
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        for c in 0..(n as usize) {
            libc::CPU_SET(c, &mut set);
        }
        libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

#[cfg(not(target_os = "linux"))]
fn pin_to_all_cores() -> bool {
    false
}

/// Run all threads concurrently, each doing an intra-thread alloc/free workload
/// (threadtest style). Returns wall-clock for the whole batch.
fn timed_mt<A: GlobalAlloc + Sync>(a: &A, threads: usize, rounds_per_thread: usize) -> Duration {
    let t = Instant::now();
    std::thread::scope(|s| {
        for tid in 0..threads {
            // Distinct per-thread seed (same across allocators for fairness).
            let seed = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(tid as u64 + 1) | 1;
            s.spawn(move || {
                let cs = workload(a, rounds_per_thread, WINDOW, seed, SMALL_MIN, SMALL_SPAN);
                black_box(cs);
            });
        }
    });
    t.elapsed()
}

fn bench_mt<A: GlobalAlloc + Sync>(name: &str, a: &A) {
    for &threads in THREAD_COUNTS {
        let _ = timed_mt(a, threads, MT_ROUNDS_PER_THREAD / 10); // warmup
        let mut samples = Vec::with_capacity(BIG_REPS);
        for _ in 0..BIG_REPS {
            samples.push(timed_mt(a, threads, MT_ROUNDS_PER_THREAD));
        }
        let (min, _med) = min_median(&samples);
        let total_ops = threads * MT_ROUNDS_PER_THREAD;
        println!(
            "{name:<14} {threads:>2} threads  min {min:>9.2?}  =>  {:7.1} Mops/s (aggregate)",
            mops(total_ops, min),
        );
    }
}

/// A live allocation handed from a producer thread to a consumer thread.
struct Block {
    ptr: *mut u8,
    layout: Layout,
}
// SAFETY: a `Block` transfers ownership of one live allocation from the producer
// to the consumer over a channel; only the consumer dereferences/frees it, so
// moving the raw pointer across threads is sound (this is exactly the
// cross-thread free pattern we want to measure).
unsafe impl Send for Block {}

/// `pairs` producer/consumer pairs run concurrently: each producer allocates and
/// hands blocks to its consumer, which frees them on a *different* thread —
/// exercising the atomic thread-free path. Bounded channel caps live memory.
fn timed_xthread<A: GlobalAlloc + Sync>(a: &A, pairs: usize, rounds_per_pair: usize) -> Duration {
    let t = Instant::now();
    std::thread::scope(|s| {
        for pid in 0..pairs {
            let (tx, rx) = sync_channel::<Block>(XT_CHAN_CAP);
            let seed = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(pid as u64 + 1) | 1;
            // Producer: allocate + send.
            s.spawn(move || {
                let mut x = seed;
                for _ in 0..rounds_per_pair {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let sz = SMALL_MIN + (x as usize % SMALL_SPAN);
                    let layout = Layout::from_size_align(sz, 16).unwrap();
                    // SAFETY: standard alloc with a valid layout.
                    let ptr = unsafe { a.alloc(layout) };
                    if ptr.is_null() {
                        continue;
                    }
                    // SAFETY: `ptr` is valid for `sz >= 1` bytes.
                    unsafe { *ptr = 1 };
                    if tx.send(Block { ptr, layout }).is_err() {
                        // Consumer vanished: free locally to avoid a leak.
                        // SAFETY: we still own `ptr` with `layout`.
                        unsafe { a.dealloc(ptr, layout) };
                    }
                }
                // tx dropped here → consumer's recv loop ends.
            });
            // Consumer: receive + free (cross-thread free!).
            s.spawn(move || {
                let mut cs = 0u64;
                while let Ok(b) = rx.recv() {
                    cs = cs.wrapping_add(b.ptr as u64);
                    // SAFETY: block came from `a` with this layout; we own it now.
                    unsafe { a.dealloc(b.ptr, b.layout) };
                }
                black_box(cs);
            });
        }
    });
    t.elapsed()
}

fn bench_xthread<A: GlobalAlloc + Sync>(name: &str, a: &A) {
    for &pairs in XT_PAIRS {
        let _ = timed_xthread(a, pairs, XT_ROUNDS_PER_PAIR / 10); // warmup
        let mut samples = Vec::with_capacity(XT_REPS);
        for _ in 0..XT_REPS {
            samples.push(timed_xthread(a, pairs, XT_ROUNDS_PER_PAIR));
        }
        let (min, _med) = min_median(&samples);
        let handoffs = pairs * XT_ROUNDS_PER_PAIR;
        println!(
            "{name:<14} {pairs:>2} pairs   min {min:>9.2?}  =>  {:7.1} Mops/s (alloc→free handoffs)",
            mops(handoffs, min),
        );
    }
}

fn main() {
    // Load the C reference allocator if MIMALLOC_C_LIB is set (run-time dlopen,
    // RTLD_LOCAL — see module docs on why we don't link it).
    let c_mi = load_c_mimalloc();
    let c_ref = c_mi.as_ref();
    if c_ref.is_some() {
        println!("targets: system (glibc) | mimalloc-c (libmimalloc) | mimalloc-rs\n");
    } else {
        println!(
            "targets: system (glibc) | mimalloc-rs   \
             (set MIMALLOC_C_LIB=<dir with libmimalloc.so> to add the C reference)\n"
        );
    }

    // ---- Phase 1: single-threaded small, pinned, interleaved, min/median ----
    let pin_core: usize = std::env::var("BENCH_PIN_CORE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let pinned = pin_to_core(pin_core);

    println!("== phase 1: single-threaded, small (8..1032) ==");
    println!(
        "{WINDOW} live, {ROUNDS} rounds, {REPS} reps{}",
        if pinned {
            format!(" (pinned to core {pin_core})")
        } else {
            " (NOT pinned — set BENCH_PIN_CORE / use taskset for stabler numbers)".to_string()
        }
    );
    run_single(c_ref, SMALL_MIN, SMALL_SPAN, WINDOW, ROUNDS, REPS);

    // ---- Phase 3a/3b: large + huge, still single-threaded & pinned ----
    println!("\n== phase 3a: single-threaded, large (16K..512K, large pages) ==");
    println!("{LARGE_WINDOW} live, {LARGE_ROUNDS} rounds, {BIG_REPS} reps");
    run_single(
        c_ref,
        LARGE_MIN,
        LARGE_SPAN,
        LARGE_WINDOW,
        LARGE_ROUNDS,
        BIG_REPS,
    );

    println!("\n== phase 3b: single-threaded, huge (1M..4M, huge path) ==");
    println!("{HUGE_WINDOW} live, {HUGE_ROUNDS} rounds, {BIG_REPS} reps");
    run_single(
        c_ref,
        HUGE_MIN,
        HUGE_SPAN,
        HUGE_WINDOW,
        HUGE_ROUNDS,
        BIG_REPS,
    );

    // Release single-core affinity so the threaded phases can use the machine.
    if pinned {
        let _ = pin_to_all_cores();
    }

    // ---- Phase 2: multi-threaded scaling (intra-thread free) ----
    println!("\n== phase 2: multi-threaded (threadtest, intra-thread free) ==");
    println!("{MT_ROUNDS_PER_THREAD} rounds/thread, {WINDOW} live/thread, min of {BIG_REPS}");
    bench_mt("system", &System);
    if let Some(c) = c_ref {
        bench_mt("mimalloc-c", c);
    }
    bench_mt("mimalloc-rs", &MiMalloc);

    // ---- Phase 4: cross-thread free (producer/consumer) ----
    println!("\n== phase 4: cross-thread free (producer allocs, consumer frees) ==");
    println!("{XT_ROUNDS_PER_PAIR} handoffs/pair, chan cap {XT_CHAN_CAP}, min of {XT_REPS}");
    bench_xthread("system", &System);
    if let Some(c) = c_ref {
        bench_xthread("mimalloc-c", c);
    }
    bench_xthread("mimalloc-rs", &MiMalloc);
}
