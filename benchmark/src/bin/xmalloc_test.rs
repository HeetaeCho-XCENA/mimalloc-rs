// SPDX-License-Identifier: MIT
//! Faithful port of mimalloc-bench `xmalloc-test` (Lever & Boreham; the
//! mimalloc-bench variant by C. Eder): `xmalloc-test/xmalloc-test.c`.
//!
//! Producer/consumer cross-thread-free stress. `workers` *allocator* threads
//! each build a `batch` of `OBJECTS_PER_BATCH` (4096) blocks, write the first
//! ≤128 bytes of each, and enqueue the batch onto a shared bounded stack
//! (`batch_count_limit = 100`). `workers` *releaser* threads dequeue batches and
//! free every block — so the thread that frees is (almost always) NOT the one
//! that allocated, exercising the remote-free path. It runs for a fixed
//! duration and reports throughput as blocks freed per second.
//!
//! Object size: `-s size` fixes the block size (default 1024); `-s -1` (size 0
//! here) draws from the C `possible_sizes` table. The C bounded queue
//! (mutex + two condvars) is reproduced with a `Mutex<Vec<Batch>>` plus a
//! `Condvar`; cross-thread free happens because releaser threads `dealloc`
//! blocks produced by allocator threads.
//!
//! PRNG substitution: the C per-thread `lran2` size picker is replaced by the
//! harness splitmix64 `pick` indexing the same `possible_sizes` table.
//!
//! Args (positional, mirroring C `-w/-t/-s`): `xmalloc_test [workers] [secs] [size]`
//! default `8 3 64` (C default `-w 4 -t 5 -s 1024`; we use a shorter smoke-able
//! default with more workers). Prints `BENCH xmalloc-test [<alloc>] FREE_PER_SEC <n>`.

use mimalloc_rs_bench::{alloc_name, arg, pick};
use std::alloc::{alloc, dealloc, Layout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

const OBJECTS_PER_BATCH: usize = 4096;
const BATCH_COUNT_LIMIT: usize = 100;
const ALIGN: usize = 16;

/// C `possible_sizes` (used when `object_size <= 0`).
const POSSIBLE_SIZES: [usize; 17] = [
    8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, (256 * 3) / 2, 512, (512 * 3) / 2, 1024,
    (1024 * 3) / 2, 2048,
];

/// One block plus its size (so the releaser can free with the right Layout).
struct Block {
    ptr: *mut u8,
    sz: usize,
}
// SAFETY: a `Block` is just an owned heap allocation handed across threads; the
// queue protocol guarantees a single owner at a time.
unsafe impl Send for Block {}

/// C `struct batch`: a fixed array of objects produced/consumed as a unit.
struct Batch {
    objects: Vec<Block>,
}

/// C bounded batch queue (mutex + empty/full condvars), as a stack.
struct Queue {
    inner: Mutex<Vec<Batch>>,
    not_empty: Condvar,
    not_full: Condvar,
    done: AtomicBool,
}

impl Queue {
    fn new() -> Self {
        Queue {
            inner: Mutex::new(Vec::new()),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            done: AtomicBool::new(false),
        }
    }

    /// C `enqueue_batch`: block while full unless done, then push + signal.
    fn enqueue(&self, batch: Batch) {
        let mut q = self.inner.lock().unwrap();
        while q.len() >= BATCH_COUNT_LIMIT && !self.done.load(Ordering::Acquire) {
            q = self.not_full.wait(q).unwrap();
        }
        q.push(batch);
        self.not_empty.notify_one();
    }

    /// C `dequeue_batch`: block while empty unless done, then pop + signal.
    fn dequeue(&self) -> Option<Batch> {
        let mut q = self.inner.lock().unwrap();
        while q.is_empty() && !self.done.load(Ordering::Acquire) {
            q = self.not_empty.wait(q).unwrap();
        }
        let b = q.pop();
        if b.is_some() {
            self.not_full.notify_one();
        }
        b
    }
}

fn main() {
    let workers = arg(1, 8).max(1);
    let secs: f64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.0);
    // 0 (or the C `-1`) selects the `possible_sizes` table.
    let object_size = arg(3, 64);

    println!(
        "xmalloc-test: workers={workers} (×2 threads) time={secs}s objSize={} batch={OBJECTS_PER_BATCH}",
        if object_size == 0 {
            "varied".to_string()
        } else {
            object_size.to_string()
        }
    );

    let queue = Queue::new();
    let counters: Vec<std::sync::atomic::AtomicU64> =
        (0..workers).map(|_| std::sync::atomic::AtomicU64::new(0)).collect();
    let start = Instant::now();

    std::thread::scope(|s| {
        // releaser (consumer) threads: dequeue + free (cross-thread).
        for w in 0..workers {
            let queue = &queue;
            let counters = &counters;
            s.spawn(move || {
                while !queue.done.load(Ordering::Acquire) {
                    if let Some(batch) = queue.dequeue() {
                        for blk in &batch.objects {
                            let layout = Layout::from_size_align(blk.sz.max(1), ALIGN).unwrap();
                            // SAFETY: `blk.ptr` was produced by an allocator
                            // thread with this exact `sz`/align; freed once.
                            unsafe { dealloc(blk.ptr, layout) };
                        }
                        counters[w].fetch_add(OBJECTS_PER_BATCH as u64, Ordering::Relaxed);
                    }
                }
            });
        }

        // allocator (producer) threads: build + write + enqueue batches.
        for w in 0..workers {
            let queue = &queue;
            s.spawn(move || {
                let mut r: u64 = w as u64 + 1; // C `lran2_init(&lr, thread_id)`
                while !queue.done.load(Ordering::Acquire) {
                    let mut objects = Vec::with_capacity(OBJECTS_PER_BATCH);
                    for i in 0..OBJECTS_PER_BATCH {
                        let sz = if object_size > 0 {
                            object_size
                        } else {
                            POSSIBLE_SIZES[(pick(&mut r) as usize) % POSSIBLE_SIZES.len()]
                        };
                        let layout = Layout::from_size_align(sz.max(1), ALIGN).unwrap();
                        // SAFETY: non-zero layout; ownership moves to the queue
                        // and is freed exactly once by a releaser thread.
                        let ptr = unsafe { alloc(layout) };
                        assert!(!ptr.is_null(), "oom");
                        // C: memset(obj, i%256, min(sz,128)).
                        let n = sz.min(128);
                        // SAFETY: `ptr` owns `sz >= n` bytes.
                        unsafe { std::ptr::write_bytes(ptr, (i % 256) as u8, n) };
                        objects.push(Block { ptr, sz });
                    }
                    queue.enqueue(Batch { objects });
                }
            });
        }

        // main thread: sleep until the deadline, then signal done + wake all.
        while start.elapsed().as_secs_f64() < secs {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        queue.done.store(true, Ordering::Release);
        queue.not_empty.notify_all();
        queue.not_full.notify_all();
    });

    // Drain any batches the releasers left behind (C does this after join).
    {
        let mut q = queue.inner.lock().unwrap();
        for batch in q.drain(..) {
            for blk in &batch.objects {
                let layout = Layout::from_size_align(blk.sz.max(1), ALIGN).unwrap();
                // SAFETY: leftover queued block, owned, freed once here.
                unsafe { dealloc(blk.ptr, layout) };
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let total: u64 = counters.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    let free_per_sec = total as f64 / elapsed;
    println!(
        "xmalloc-test: {total} blocks freed in {elapsed:.3}s ({:.3} M free/sec)",
        free_per_sec * 1e-6
    );
    eprintln!(
        "BENCH xmalloc-test [{}] FREE_PER_SEC {:.0}",
        alloc_name(),
        free_per_sec
    );
}
