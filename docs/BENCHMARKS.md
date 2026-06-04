<!-- SPDX-License-Identifier: MIT -->
# Benchmarks

mimalloc-rs is a **rust-native** engine: its best (and intended) deployment is
compiled into a Rust program as the static `#[global_allocator]`, where the
whole alloc/free chain inlines into the caller. These are the numbers that
matter; an `LD_PRELOAD`/C-replacement build is out of scope (it lives on the
`export` branch and pays an unavoidable `.so`-boundary cost).

All numbers are on a pinned, quiet **i7-14700K** (cores 2–9), interleaved median.

## Peak-vs-peak — mimalloc's official `test-stress` workload

The fairest comparison runs mimalloc's own `test/test-stress.c` workload with
**each allocator in its own best-case build**, both fully inlined:

- **mimalloc-rs (peak):** `examples/stress.rs` (a faithful Rust port of
  `test-stress.c`) with rs as the static `#[global_allocator]`, release LTO.
- **mimalloc-c (peak):** `test-stress.c` compiled with mimalloc's single-source
  `src/static.c` at `-O3 -flto` (so `mi_malloc` inlines into the bench).

Both use the default per-thread heap (rolling heaps off); same cores; interleaved.

| config (threads / scale / iters) | rs vs C |
|---|---|
| 1 / 50 / 100  (single-threaded) | **+1.2%** |
| 8 / 50 / 50 | **−1.7%** (rs faster) |
| 8 / 100 / 50 | **+1.5%** |
| 16 / 50 / 30 | **+1.1%** |
| 4 / 100 / 50 | **+4.2%** |

**On mimalloc's own stress benchmark, at peak-vs-peak, mimalloc-rs is on par
with mimalloc-c** — within ~±1.5% across thread counts (slightly faster at 8T).
A from-scratch Rust port and the C original are equivalent when each runs in its
native best-case build; there is no fundamental Rust-vs-C cost here.

### Reproduce

```sh
# rs peak
cargo build --release --example stress
# C peak (point at a mimalloc v3 checkout)
gcc -O3 -DNDEBUG -flto -I <mimalloc>/include \
    <mimalloc>/test/test-stress.c <mimalloc>/src/static.c -lpthread -latomic -o stress_c
# compare (pin + interleave); disable the C build's MI_USE_HEAPS for parity
taskset -c 2-9 ./target/release/examples/stress 8 50 50
taskset -c 2-9 ./stress_c                        8 50 50
```
`examples/stress.rs` takes `THREADS SCALE ITER [NUMA_NODE]`; it prints
`STRESS_SECONDS <n>` to stderr and binds memory + CPU to `NUMA_NODE` on Linux.

## Microbenchmark suite (`benchmark/`)

`benchmark/` is an isolated cargo project with faithful Rust ports of five
mimalloc-bench microbenchmarks, run with mimalloc-rs as the static
`#[global_allocator]` (peak). A `bench-system` feature rebuilds the *same*
binaries on Rust's `System` (glibc) allocator, and `benchmark/run.sh` adds the
mimalloc-c peak (original C bench + `src/static.c -O3 -flto`) for a 3-way table:

```sh
MI_SRC=~/repos/mimalloc-v3 CBENCH=/path/to/mimalloc-bench/bench bash benchmark/run.sh
```

Results (pinned i7-14700K, cores 2–9, interleaved median ×5, 8 threads):

| workload | metric | mimalloc-rs | system (glibc) | mimalloc-c |
|---|---|---|---|---|
| xmalloc-test (producer/consumer cross-thread free) | free/sec ↑ | **235 M** | 60 M | 62 M |
| larson (server MT) | ops/sec ↑ | **304 M** | 94 M | 87 M |
| alloc-test (fast path) | sec ↓ | **0.12** | 0.14 | — |
| cache-thrash (false-share, 1 B) | sec ↓ | 0.11 | 0.09 | 0.09 |
| malloc-large (5–25 MiB) | sec ↓ | 2.06 | 1.97 | 2.33 |

On the multi-threaded / contended patterns mimalloc-rs is **~3–4× glibc** and
**on par with or ahead of mimalloc-c**; glibc's direct `mmap` edges it on pure
huge allocation. See `benchmark/README.md` for per-bench detail and caveats.

## Criterion micro-benchmarks

`benches/alloc.rs` tracks the allocator's own alloc/free cost across size classes
and a few patterns, with mean/median/CI/outlier stats — the local
micro-regression gate during refactors:

```sh
cargo bench --bench alloc -- --save-baseline before   # before a change
cargo bench --bench alloc -- --baseline before        # after
```

## No-regression gate (for perf-affecting changes)

Build `examples/stress` on the change branch **and** on `main`, run both
interleaved (median, pinned), and require no regression — plus `cargo bench
--bench alloc` against a saved baseline.
