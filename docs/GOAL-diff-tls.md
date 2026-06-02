<!-- SPDX-License-Identifier: MIT -->
# GOAL — Differential coverage + nightly TLS fast path

> Round brief for `omc ultragoal`. Two independent stories:
> **C1** widen the C-vs-rs differential harness (the safety net for all future
> perf/feature work); **A1a** add a nightly `#[thread_local]` cached default-heap
> pointer so the small-object hot path stops paying the `thread_local!` guard.
> Faithful to mimalloc v3, idiomatic + simple Rust, and the absolute
> no-regression bar (authoritative timing on the pinned i7-14700K via
> `scripts/perf_compare.sh`; this env is too noisy for timing).

## Context / current state

- `tests/differential.rs` already runs a deterministic alloc/calloc/realloc/
  aligned/free/usable workload through **both** C `libmimalloc` (oracle) and
  mimalloc-rs and asserts the observable contract (alignment, calloc/zalloc
  zeroing, `usable_size >= size`, no live-block overlap, realloc preserves
  content, no final corruption, version match). Gated on `feature = "differential"`
  + `have_c_mimalloc` (build.rs sets it when `MIMALLOC_C_LIB` is provided).
  **Gaps**: single-threaded only; sizes capped at ~8 KiB (small/medium only — the
  large ≥16 KiB and huge ≥512 KiB paths are never differentially exercised);
  narrow API surface; not run in CI.
- `src/init.rs::tls` uses `std::thread_local! { static DEFAULT_HEAP: Heap }` and
  every `malloc`/`malloc_aligned`/`zalloc`/`free`(tid) goes through `.with()`,
  which carries a lazy-init/state guard. The `nightly` feature exists but does
  **not** wire a `#[thread_local]` malloc path (so the earlier "nightly = no
  change" perf result was a null test, not evidence — see `docs/perf-hotpath.md`
  C2). mimalloc-C uses a `__thread mi_heap_t*` cached pointer.

## Principles / bar (unchanged)

- Faithful v3 behavior; idiomatic, simple Rust; `// SAFETY:` on new unsafe.
- **No perf regression on any `bench_suite` phase** vs the pre-round `main`
  (pinned machine). **Stable builds must be byte-for-byte behavior-unchanged** by
  A1a (the new path is `cfg(feature = "nightly")` only).
- A1a is **measure-and-keep**: land it only if the pinned machine shows a
  phase-1 improvement (or clear neutrality with a path to a win); otherwise park
  it with the measurement recorded.

## Stories

### C1 — widen the differential harness  *(low risk; tests only)*
- **Sizes across all bins**: drive sizes that cross small→medium→large→huge
  (e.g. mix in occasional ≥16 KiB and ≥512 KiB up to a few MiB), so the
  large-page and huge paths are differentially validated. Keep the bulk small
  (cheap) with a low-probability large/huge tail.
- **Broader API surface** (assert the same contract both sides): `free(NULL)`
  no-op, `realloc(NULL, n)` == malloc, `realloc(p, 0)` behavior, `realloc` both
  growing and shrinking across a bin boundary, `calloc` overflow → null,
  large alignments (> page, e.g. 64 KiB), `usable_size(NULL)` == 0,
  `zalloc`/`calloc` zeroing at huge sizes.
- **Multi-threaded cross-thread differential**: a bounded workload where blocks
  allocated on one thread are freed on another (the same producer/consumer shape
  as `bench_suite` phase 4), run through both allocators, asserting no overlap
  within each allocator, content integrity across the hand-off, and no leak.
  Drive our side through the Rust API; keep it deterministic (seeded, fixed
  thread/op counts) so it is reproducible.
- **CI wiring**: add a `differential` CI job that builds C `libmimalloc` (or
  uses a cached/prebuilt one) and runs `cargo test --features differential
  --test differential`; if building the C lib in CI is impractical, document the
  local command and gate the job on the lib being available (skip cleanly
  otherwise) so CI never silently tests rs-against-itself.
- **DoD**: extended harness passes for both C (oracle) and rs locally with
  `MIMALLOC_C_LIB` set; existing tests unchanged; clippy/fmt clean; the new MT
  differential runs under the normal + (where feasible) `secure`/`debug` builds.

### A1a — nightly `#[thread_local]` cached heap/tid fast path  *(nightly-only)*
- Under `cfg(feature = "nightly")`, add a real `#[thread_local] static` caching
  the calling thread's default-heap pointer (and tid) and have
  `init::malloc*/zalloc/free` read it directly, bypassing the `thread_local!`
  `.with()` guard on the hot path. On stable (no `nightly`) the existing
  `thread_local!` path is used **unchanged**.
- **Lifecycle correctness** (the hard part): the per-thread `Heap` storage and
  its `Drop` (thread-exit page hand-off via `Heap::drop`, see `crate::heap`/
  `crate::subproc`) must still run exactly once at thread exit. The
  `#[thread_local]` slot is a *cache pointer* into the owning storage (mirroring
  mimalloc-C's `__thread mi_heap_t*`); first access lazily initializes it; it
  must be sound during init-before-main (GlobalAlloc reentrancy) and never form
  a `&Heap` that outlives the thread. Reuse the existing `process_keys`/
  `current_tid` bootstrap.
- **DoD**: stable build & behavior unchanged (diff shows only `cfg(nightly)`
  additions on the hot path); under `--features nightly`: default tests, capi,
  loom, and the secure+debug abort gate all green, the extended differential
  passes, and `scripts/perf_compare.sh` on the pinned machine shows phase 1
  improved (or neutral) with **no regression on any phase**. Keep only if it
  helps; otherwise park with the recorded measurement.

## Verification

- `cargo test` (stable + `--features nightly`), capi, loom, secure+debug abort
  gate (×15, 0), clippy (all hardened features), fmt.
- `MIMALLOC_C_LIB=<v3 build> cargo test --features differential --test differential`.
- Pinned machine: `scripts/perf_compare.sh 9 2` (stable baseline) and a
  nightly-vs-nightly comparison for A1a; RSS bench unaffected.
