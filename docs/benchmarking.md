<!-- SPDX-License-Identifier: MIT -->
# Benchmarking

How to measure mimalloc-rs reproducibly. **Timing is never trusted from CI**
(shared runners are too noisy); always measure on a quiet, **pinned** machine and
take the best of several runs.

## Harnesses

| Tool | Purpose | Command |
|---|---|---|
| `scripts/perf_compare.sh` | **branch-vs-`main` no-regression gate** — per-phase min/median Δ% | `MIMALLOC_C_LIB=<dir> scripts/perf_compare.sh 9 2` |
| `examples/bench_suite.rs` | end-to-end vs system + C `libmimalloc` (phases: small / large / huge / threadtest / cross-thread) | `MIMALLOC_C_LIB=<dir> cargo run --release --example bench_suite` |
| `benches/alloc.rs` (Criterion) | statistical micro-benchmarks across size classes + patterns | `cargo bench --bench alloc` |
| `examples/microbench.rs` | single-allocator instruction-count A/B (`perf stat`) | `MB_ALLOC=rs perf stat -- ./target/release/examples/microbench` |
| `examples/rss_spike.rs` | proves delayed purge returns memory to the OS (RSS) | `cargo run --release --example rss_spike` |
| `scripts/mimalloc-bench.sh` | the standard cross-allocator suite (cfrac, larson, mstress, …) vs system + C | `MIMALLOC_C_LIB=<dir> scripts/mimalloc-bench.sh` |
| `examples/profile_alloc.rs` | rs-only hot loop for flamegraphs / `perf record` | see `docs/perf-hotpath.md` |

## The no-regression protocol (required for perf-affecting changes)

```sh
# On the pinned machine, on your branch:
MIMALLOC_C_LIB=/path/to/mimalloc-v3/build scripts/perf_compare.sh 9 2
```

It builds the release `bench_suite` for the current branch and for `main` (in a
throwaway worktree), runs them interleaved on a pinned core, and prints per-phase
`min/median` and a `REGRESS/ok/faster` verdict. A change merges only if **no
phase shows `REGRESS`** (both Δmin and Δmed beyond the noise threshold). Prefer
the `min`; single-thread and cross-thread phases are the noisiest — bump `REPS`
to 15–21 and minimize background load.

## Criterion micro-regression tracking

```sh
cargo bench --bench alloc -- --save-baseline before
# ... make a change ...
cargo bench --bench alloc -- --baseline before        # prints per-bench deltas
```

## RSS

```sh
cargo run --release --example rss_spike                       # purge on  → RSS drops
MIMALLOC_PURGE_DELAY=-1 cargo run --release --example rss_spike  # off → RSS stays (control)
```

## Notes / caveats

- **Linux residency**: on overcommit Linux `commit` is `mprotect`; pages become
  resident on first touch, so the lever for RSS is **purge** (`MADV_DONTNEED`),
  not commit. See `docs/perf-hotpath.md` §C2 and the purge round notes.
- The known small-object hot-path gap vs C and its analysis (full-page eviction,
  inlining; TLS ruled out) live in `docs/perf-hotpath.md`.
