<!-- SPDX-License-Identifier: MIT -->
# Contributing to mimalloc-rs

Thanks for your interest! mimalloc-rs is a from-scratch Rust re-implementation of
the **mimalloc v3** allocator (`MI_MALLOC_VERSION 30302`) — not an FFI binding.
The goal is to be a faithful, idiomatic, and verifiable port. Three principles
guide every change:

1. **Faithful to v3** — match mimalloc v3's design and observable behavior; cite
   the C source (`src/*.c`) when porting.
2. **Idiomatic Rust** — `Option`/`Result`/`NonNull`, RAII, encapsulated `unsafe`.
3. **Simple & clean** — the smallest correct mechanism; no speculative abstraction.

## Dev setup

- **MSRV: Rust 1.84** (strict-provenance `ptr` APIs). Don't raise it casually.
- Stable toolchain for most work; **nightly** for the `nightly` feature, Miri,
  sanitizers, and fuzzing.
- The C reference lives at a separate `mimalloc` v3.3.2 checkout; build it as a
  shared lib and point `MIMALLOC_C_LIB` at the dir containing `libmimalloc.so`
  for the differential and benchmark comparisons.

## The gate checklist (run before opening a PR)

Correctness — must all pass (CI enforces these):

```sh
cargo build && cargo build --no-default-features          # std + no_std core
cargo test                                                # default
cargo test --features secure,debug,stats,track            # hardened + stats
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features capi,secure,debug,stats,track -- -D warnings
```

Concurrency / UB / contract (see `docs/verification.md` for the full matrix):

```sh
RUSTFLAGS="--cfg loom" cargo test --lib loom_tests        # loom models
cargo +nightly miri test --lib bitmap                     # + free_list / layout / bits
MIMALLOC_C_LIB=<dir> cargo test --features differential --test differential
cargo +nightly fuzz run alloc -- -runs=200000             # ASan op-stream fuzz
```

The secure+debug **abort gate** (double/invalid-free detection) is exercised by
running the hardened test suite; CI runs ASan + ThreadSanitizer jobs too.

## Performance changes — the no-regression rule

Performance is **never gated in CI** (shared runners are too noisy). The bar is:

> A change must show **no regression on any `bench_suite` phase** vs `main`,
> measured on a quiet, **pinned machine**.

```sh
MIMALLOC_C_LIB=<dir> scripts/perf_compare.sh 9 2          # branch vs main, per-phase
```

Merge a perf-affecting change only after a clean pinned-machine run. For the
standard cross-allocator suite and RSS, see `docs/benchmarking.md`
(`scripts/mimalloc-bench.sh`, `examples/rss_spike.rs`). When a perf idea turns
out neutral or worse, **park it and record why** (see `docs/perf-hotpath.md`).

## `unsafe` policy

`#![forbid(unsafe_code)]` is impossible for an allocator, so instead:

- Every `unsafe` block carries a `// SAFETY:` comment justifying it.
- `unsafe_op_in_unsafe_fn` is enabled — be explicit inside `unsafe fn`.
- Pointer code uses strict-provenance APIs (`addr`/`with_addr`/`expose_provenance`),
  validated under Miri `-Zmiri-strict-provenance`.

## PRs

- One logical change per PR; keep diffs small and reviewable.
- Match the surrounding code's style, comment density, and naming.
- New behavior needs tests; new `unsafe` needs SAFETY comments + (where it's
  concurrent) a loom model and/or a TSan-covered stress test.
- Fill in the PR template (what / why / verification).

See also: `docs/verification.md`, `docs/benchmarking.md`, `docs/perf-hotpath.md`.
