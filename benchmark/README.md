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

## Benchmarks (the 5 distinct allocator patterns)

| bin | mimalloc-bench source | exercises |
|---|---|---|
| `malloc_large` | `malloc-large.cpp` | large/huge (5–25 MiB), commit/purge |
| `cache_thrash` | `cache-thrash.cpp` (Hoard) | per-thread cache locality / false-sharing |
| `alloc_test` | `allocator_tester.{h,cpp}` (rpmalloc) | single-thread-ish fast-path throughput, Pareto sizes |
| `xmalloc_test` | `xmalloc-test.c` | producer/consumer **cross-thread free** |
| `larson` | `larson.cpp` | server-style MT alloc/free with slot churn |

Each port cites its C source, reproduces the size distribution / threading /
alloc-free-transfer pattern, and substitutes the C PRNG with splitmix64 over the
same ranges (documented in each file's header).

## Run

```sh
# rs vs system, individually:
cargo build --release && ./target/release/xmalloc_test
cargo build --release --features bench-system && ./target/release/xmalloc_test

# full 3-way table (builds rs + system + the C-peak variants, medians):
MI_SRC=~/repos/mimalloc-v3 CBENCH=/path/to/mimalloc-bench/bench REPS=5 bash run.sh
```

## Sample results (pinned i7-14700K, cores 2–9, interleaved median, 8T)

Both allocators are compared to glibc **on their own faithful harness** (a
same-binary allocator swap), and the cross-language comparison is the **speedup
over glibc** — the two harnesses are different programs of the same pattern, so
the absolute rs-vs-mimalloc-c numbers are not directly comparable.

| pattern | mimalloc-rs / glibc | mimalloc-c / glibc |
|---|---|---|
| xmalloc-test (producer/consumer cross-thread free) | **3.9×** | **4.8×** |
| alloc-test (fast path) | **1.27×** | **1.27×** |
| larson (server MT) | **1.19×** | 1.04× |
| malloc-large (5–25 MiB) | **0.95×** | 1.11× |
| cache-thrash (false-share, 1 B) | **0.82×** | 1.00× |

**Reading it:**
- The big win for *both* allocators over glibc is **xmalloc-test** (pure
  cross-thread free, glibc's weak spot): rs ~3.9×, mimalloc-c ~4.8×.
- mimalloc-rs and mimalloc-c are otherwise **in the same league** vs glibc:
  alloc-test tie (1.27×), larson rs slightly ahead.
- **mimalloc-rs is slower than glibc** on **malloc-large** (0.95×) and
  **cache-thrash** (0.82×) — both cases where mimalloc-c stays ≥ glibc, so they
  are genuine rs weak spots (large-block/purge handling; tiny-object placement).
  cache-thrash at 1-byte objects is largely write-loop-bound, so its number mixes
  placement with noise.

> **Methodology note (important).** The `mimalloc-c` column is measured by
> `LD_PRELOAD`-ing a real mimalloc `.so` (CMake Release) over the **original**
> mimalloc-bench binaries — `run.sh` does this. Do **not** link mimalloc's
> `static.c` into these C benches: they call `malloc`/`new`, which static-link to
> **glibc** unless interposed, silently turning the "mimalloc-c" column into
> glibc. (An earlier version of this file made exactly that mistake.)
