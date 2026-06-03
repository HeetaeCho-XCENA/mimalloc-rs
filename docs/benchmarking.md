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
| `examples/stress.rs` | **peak-vs-peak** — Rust port of mimalloc's official `test/test-stress.c`, run with rs as the static `#[global_allocator]` (best case) | `cargo build --release --example stress && ./target/release/examples/stress [THREADS] [SCALE] [ITER] [NUMA_NODE]` |

## Peak-vs-peak: the official `test-stress` workload (`examples/stress.rs`)

`examples/stress.rs` is a faithful Rust port of mimalloc's own
`test/test-stress.c` (same `splitmix64` PRNG, `pick`/`chance` distribution,
cookie-verified `alloc_items`, shared transfer buffer, retained objects, and
`ITER` thread re-creation rounds). Running it with mimalloc-rs as the static
`#[global_allocator]` (LTO, fully inlined) measures the allocator in its **best**
environment — unlike the `LD_PRELOAD` cdylib, which crosses a `.so` export
boundary and so cannot inline into the caller.

For a fair best-vs-best vs C, build a `test-stress` binary that links mimalloc
statically with LTO (so `mi_malloc` likewise inlines), e.g.:

```sh
# C peak: test-stress.c + mimalloc's single-source static.c, inlined via LTO
gcc -O3 -DNDEBUG -flto -I <mimalloc>/include \
    <mimalloc>/test/test-stress.c <mimalloc>/src/static.c -lpthread -latomic -o stress_c
# rs peak:
cargo build --release --example stress
# Compare (pin + interleave; both use the default per-thread heap — disable the
# C build's MI_USE_HEAPS for parity):
taskset -c 2-9 ./stress_c              8 50 50
taskset -c 2-9 ./target/release/examples/stress 8 50 50
```

Args: `THREADS SCALE ITER [NUMA_NODE]`. `SCALE > 100` enables very large
objects. `NUMA_NODE` (Linux) binds memory (`set_mempolicy(MPOL_BIND)`) and thread
CPU affinity to that node. On the pinned i7-14700K, rs-peak lands within ~±1.5%
of C-peak across thread counts (slightly faster at 8T) — confirming the two
allocators are on par when each runs in its native best-case build.

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

## Preload TLS model (fair LD_PRELOAD comparison)

For an apples-to-apples comparison both allocators must be loaded the same way —
**`LD_PRELOAD`-ed shared objects** (not the C lib via `dlopen` function pointers,
which adds indirection and flatters mimalloc-rs). When the preload `cdylib` is
built on a **nightly** toolchain, `scripts/mimalloc-bench.sh` and
`scripts/preload-check.sh` add `-Z tls-model=initial-exec`, matching
mimalloc-C's `MI_TLS_MODEL`: a Rust `cdylib` otherwise defaults to the
general-dynamic TLS model, whose `__tls_get_addr` call lands on every
malloc/free. initial-exec removes it (≈+7% on larson/cfrac; see
`docs/perf-hotpath.md` §C2-update). It is safe because a preloaded library is
loaded at startup. To reproduce by hand:

```sh
RUSTFLAGS="--cfg override_export -Z tls-model=initial-exec" \
  cargo rustc --release --features override --crate-type cdylib --target-dir target/preload
```

This affects only the preload `cdylib`; statically-linked `#[global_allocator]`
use already gets the fast local-exec model, so `perf_compare.sh` is unaffected.

## Notes / caveats

- **Linux residency**: on overcommit Linux `commit` is `mprotect`; pages become
  resident on first touch, so the lever for RSS is **purge** (`MADV_DONTNEED`),
  not commit. See `docs/perf-hotpath.md` §C2 and the purge round notes.
- The known small-object hot-path gap vs C and its analysis (full-page eviction,
  inlining; TLS model on the preload path) live in `docs/perf-hotpath.md`.
