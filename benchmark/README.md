<!-- SPDX-License-Identifier: MIT -->
# mimalloc-rs-bench

Faithful Rust ports of mimalloc-bench microbenchmarks, run with **mimalloc-rs as
the static `#[global_allocator]`** (its peak — fully inlined). A 3-way harness
compares, on the same workload pattern:

- **mimalloc-rs** — these Rust binaries (default build).
- **system (glibc)** — the *same* Rust binaries built `--features bench-system`
  (swaps `#[global_allocator]` to `std::alloc::System`). rs-vs-system is therefore
  on identical code.
- **mimalloc-c** — the original mimalloc-bench C/C++ source linked with mimalloc's
  `src/static.c` at `-O3 -flto` (peak, inlined). Different code (each language's
  native impl), so it compares cleanly only where the metric is workload-rate or
  the parameters match.

Isolated from the parent crate (its own `[workspace]`), so it never affects the
library build/CI. Not published.

## Benchmarks (the allocator patterns)

| bin | mimalloc-bench source | exercises |
|---|---|---|
| `malloc_large` | `malloc-large.cpp` | large/huge (5–25 MiB), commit/purge |
| `cache_thrash` | `cache-thrash.cpp` (Hoard) | per-thread cache locality / false-sharing |
| `alloc_test` | `allocator_tester.{h,cpp}` (rpmalloc) | single-thread-ish fast-path throughput, Pareto sizes |
| `xmalloc_test` | `xmalloc-test.c` | producer/consumer **cross-thread free** |
| `larson` | `larson.cpp` | server-style MT alloc/free with slot churn |
| `calloc_test` | — (ours) | `calloc`/zeroed alloc + realistic first-touch |
| `realloc_test` | — (ours) | `realloc` grow churn across size classes |

### Coverage matrix

What the suite spans, so additions stay principled rather than ad-hoc. Rows are
allocator behaviours; a cell names the workload(s) that exercise it.

| behaviour \ size | tiny ≤16 B | small ≤1 KiB | medium ≤84 KiB | large ≤512 KiB | huge >512 KiB |
|---|---|---|---|---|---|
| single-thread malloc/free churn | `alloc_test`(1T) | `alloc_test` | `alloc_test` | — | — |
| multi-thread random churn | `cache_thrash` | `larson` | `larson` | — | `malloc_large`* |
| **cross-thread free** (producer/consumer) | `xmalloc_test` | `xmalloc_test` | — | — | — |
| alloc + replace (long-lived set) | — | — | — | `malloc_large` | `malloc_large` |
| **calloc / zeroed** (+ first-touch) | `calloc_test` | `calloc_test` | `calloc_test` | `calloc_test` | `calloc_test` |
| **realloc** grow/shrink | `realloc_test` | `realloc_test` | `realloc_test` | `realloc_test` | — |
| per-thread cache locality | `cache_thrash` | — | — | — | — |

`*` `malloc_large` is single-thread. Axes intentionally **not** benched (covered
by correctness tests, not perf-critical): `aligned_alloc` placement, huge
cross-thread free, `no_std` `Heap` API. Every memory-touching workload reads/writes
its blocks so first-touch faults — the real cost — are counted, not hidden behind
never-used allocations.

Each port cites its C source, reproduces the size distribution / threading /
alloc-free-transfer pattern, and substitutes the C PRNG with splitmix64 over the
same ranges (documented in each file's header).

## Run

```sh
# Portable: run the WHOLE suite anywhere (only needs cargo + this repo).
# Builds rs + system(glibc) and prints an rs-vs-glibc table.
bash bench-all.sh

# Tune it (all optional); add a mimalloc-c column with a prebuilt .so or a checkout:
T=8 REPS=5 SCALE=1 PIN="taskset -c 2-9" \
  MI_SO=/path/to/libmimalloc.so bash bench-all.sh
MI_SRC=~/repos/mimalloc bash bench-all.sh     # builds the .so via cmake

# rs vs system, a single workload by hand:
cargo build --release && ./target/release/xmalloc_test
cargo build --release --features bench-system && ./target/release/xmalloc_test
```

`bench-all.sh` is the cross-machine runner (no external benchmarks needed —
the mimalloc-c column comes from `LD_PRELOAD`-ing a real mimalloc `.so` over the
*same* system binary). `run.sh` is the stricter cross-check that compares against
the **original** mimalloc-bench C binaries (needs them built); use it on the
reference box.

## Sample results (pinned i7-14700K, cores 2–9, interleaved median, 8T)

Both allocators are compared to glibc **on their own faithful harness** (a
same-binary allocator swap), and the cross-language comparison is the **speedup
over glibc** — the two harnesses are different programs of the same pattern, so
the absolute rs-vs-mimalloc-c numbers are not directly comparable.

| pattern | mimalloc-rs / glibc | mimalloc-c / glibc |
|---|---|---|
| xmalloc-test (producer/consumer cross-thread free) | **3.8×** | **4.9×** |
| alloc-test (fast path) | **1.17×** | **1.28×** |
| larson (server MT) | **1.20×** | 1.08× |
| malloc-large (5–25 MiB) | **0.97×** | 1.11× |
| cache-thrash (false-share, 1 B) | **0.91×** | 1.00× |
| calloc-test (zeroed + touch, 64 KiB) | **0.88×** | 1.00× |
| realloc-test (grow churn) | **0.86×** | 0.86× |

**Reading it:**
- The big shared win over glibc is **xmalloc-test** (pure cross-thread free,
  glibc's weak spot): rs ~3.8×, mimalloc-c ~4.9×.
- mimalloc-rs and mimalloc-c are **in the same league** vs glibc; rs leads on
  larson, ties on the rest.
- **calloc-test** is near parity once the memory is *touched* (the realistic
  case): the kernel's first-touch faulting dominates and is allocator-independent.
  Measuring zeroed allocations that are never used would make rs look
  artificially fast (it skips the memset and never faults the pages) — this
  workload deliberately touches every page.
- **realloc-test** and **malloc-large**/**cache-thrash** are the spots where rs
  sits at or just under glibc; rs tracks mimalloc-c there (realloc 0.86× = mi-c),
  so it is not a Rust-specific cost. cache-thrash at 1 B is largely write-loop
  noise (parity at ≥16 B).

> **Methodology note (important).** The `mimalloc-c` column is measured by
> `LD_PRELOAD`-ing a real mimalloc `.so` (CMake Release) over the **original**
> mimalloc-bench binaries — `run.sh` does this. Do **not** link mimalloc's
> `static.c` into these C benches: they call `malloc`/`new`, which static-link to
> **glibc** unless interposed, silently turning the "mimalloc-c" column into
> glibc. (An earlier version of this file made exactly that mistake.)
