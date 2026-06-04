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

## Sample results (pinned i7-14700K, cores 2–9, interleaved median ×5, 8T)

| workload | metric | mimalloc-rs | system (glibc) | mimalloc-c |
|---|---|---|---|---|
| xmalloc-test (producer/consumer) | free/sec ↑ | **235 M** | 60 M | 62 M |
| larson (server MT) | ops/sec ↑ | **304 M** | 94 M | 87 M |
| alloc-test (fast path) | sec ↓ | **0.12** | 0.14 | — (C iters differ) |
| cache-thrash (false-share, 1 B) | sec ↓ | 0.11 | 0.09 | 0.09 |
| malloc-large (5–25 MiB) | sec ↓ | 2.06 | 1.97 | 2.33 |

**Reading it:**
- On **contended / multi-threaded** patterns mimalloc-rs is **decisively faster
  than glibc** (xmalloc-test ~3.9×, larson ~3.2×, alloc-test ~1.15×) and **matches
  or beats mimalloc-c** (xmalloc-test, larson, malloc-large).
- On **pure huge allocation** (malloc-large) glibc's direct `mmap` is ~5% ahead;
  rs still beats mimalloc-c there.
- **cache-thrash** with 1-byte objects is dominated by the write loop, not
  allocation; rs is ~within noise of glibc/C.

The takeaway matches the project's scope: as a Rust-native `#[global_allocator]`,
mimalloc-rs is on par with mimalloc-c and a large win over the default allocator
on the multi-threaded patterns that matter.

> Caveat: `rs` vs `system` is a perfectly fair same-binary comparison. The
> `mimalloc-c` column runs a different (C) implementation of the same pattern; it
> is most directly comparable on the rate-based benches (xmalloc-test, larson)
> and the matched-parameter ones (malloc-large, cache-thrash). alloc-test's C
> original hardcodes a much larger iteration count, so only rs-vs-system is shown.
