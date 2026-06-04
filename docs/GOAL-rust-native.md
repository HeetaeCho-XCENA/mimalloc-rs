<!-- SPDX-License-Identifier: MIT -->
# GOAL — rust-native mimalloc engine

**SSOT for this round.** Supersedes the perf-gap / LD_PRELOAD framing: the project
is refocused on being a **rust-native engine**, not a C-allocator replacement.

## 1. Vision

mimalloc-rs is the **fastest way to use mimalloc from Rust**: a faithful Rust
port of the mimalloc v3 engine, optimized for the case where it is compiled
**into** a Rust program (static `#[global_allocator]`) — its best-performing
environment. It is **composable** (per-container `Allocator`, not only the global
one) and **extensible** (a `#![no_std]` engine with a `Prim` OS-backend trait).

The benchmark evidence drove this: mimalloc-rs matches or beats mimalloc-c only
when built as a native Rust engine (inlined, local-exec TLS); as an `LD_PRELOAD`
drop-in it pays an unavoidable `.so`-boundary cost. So we stop chasing the
C-replacement use case and double down on the native one.

### Use cases (in scope)
- **Primary**: `#[global_allocator] static A: MiMalloc = MiMalloc;`
- **Composable**: `Vec::with_capacity_in(n, MiMalloc)` / `Box::new_in(_, MiMalloc)`
  via the `Allocator` trait (stable `allocator-api2` + nightly `core::alloc`).
- **Extensible / embeddable**: the `#![no_std]` core engine (`Heap`, `Arena`,
  `Prim`) usable without `std`.

### Out of scope (archived, not deleted — see §4)
- C ABI (`mi_*`), libc-symbol `LD_PRELOAD` override, C++ `operator new/delete`,
  foreign-pointer fallback, and the preload-only full-page eviction. These move
  to the `export` branch; anyone needing a C/preload build uses that branch.

## 2. Headline principles — **rust-native** (most important this round)

1. **Rust-idiomatic**: lean on the language's strengths — compile-time safety,
   the type system, ownership/RAII, and **trait extensibility** (`Prim`,
   `GlobalAlloc`, `Allocator`) — rather than transliterating C.
2. **Minimize `unsafe`**: the port currently carries a large `unsafe` surface.
   Refactor to cut it **drastically** — encapsulate the irreducible unsafe
   (atomics, OS memory, free-list/page-map pointer encoding) behind safe,
   invariant-checked abstractions so call sites are safe; prefer references and
   checked access where aliasing/provenance allow. `#![forbid(unsafe_code)]` is
   impossible for an allocator, so the metric is **unsafe-block/`unsafe fn`
   count and surface**, tracked before/after. **Absolute constraint: zero
   benchmark regression** and Miri (strict-provenance) stays green.
3. **Simple & clean, intuitive API**: the smallest, clearest public surface for
   the use cases above; remove indirection that only served the C/preload paths.
4. **Minimal comments**: do **not** re-explain mimalloc's internal algorithms —
   a reader who knows mimalloc does not need them, and `docs/FIDELITY.md` holds
   the C↔rs mapping. Comment **only** the points where the port **deviates from
   C in a Rust-idiomatic way** (e.g. why a `Cell`/`AtomicPtr`/newtype, an RAII
   `Drop`, a `const` table). `// SAFETY:` comments stay (mandatory for the
   remaining `unsafe`); terse C-source name pointers may stay where they aid
   verification, but verbose internal-logic prose is removed.

## 3. Keep (core rust-native engine)

- Engine: `arena`, `arena_meta`, `bitmap`, `page`, `page_map`, `page_queue`,
  `free_list`, `heap` (minus eviction), `subproc`, `os`, `prim`, `init`
  (TLS heap), `bits`, `layout`, `sync`, `atomic`, `options` (wired subset),
  random keys.
- `api::MiMalloc`: `GlobalAlloc` (primary) **and** `Allocator` (composable).
- Rust API: `malloc` / `free` / `realloc` / `zalloc` / `alloc_aligned`, `Heap`.
- **Cross-thread free + thread-exit abandoned reclaim** (FE0/FE1) — core to any
  multithreaded Rust program (distinct from the eviction being removed).
- `#![no_std]` core (the `std` feature stays default-on; the engine stays
  no_std-capable — a core goal).
- Delayed purge / RSS return (PC0–PC3).
- NUMA + huge **OS** pages (helps the NUMA-server deployment; distinct from the
  `MI_BIN_HUGE` size class, which is core regardless).
- `secure` / `debug` hardening, `stats`, `track` — all feature-gated, off by
  default (zero cost in the default build).
- Verification: `differential` (vs C v3.3.2, as a reference), loom, TSan, Miri.

## 4. Archive to `export` branch, then remove from `main`

`export` branch = a snapshot of the current full-featured HEAD (C ABI +
`LD_PRELOAD` + eviction). It is the permanent home of the C/preload build.

Removed from `main`:
- `capi.rs` (C ABI `mi_*`, explicit `mi_heap_*`, `mi_new` family) + `capi` feature.
- `override_symbols.rs` (libc `malloc`/`free`/… + C++ `operator new`/`delete`)
  + `override` feature + the cdylib / `-Z tls-model=initial-exec` recipe.
- `sysalloc.rs` (foreign-pointer fallback).
- **Full-page eviction (FE2)**: the `override_export`/`test` `find_free_page`
  variant, `page_to_full`, `reclaim_on_free` (+ `init::try_reclaim_on_free` and
  the `free_try_collect_mt` reclaim step), `Opt::PageFullRetain`. (Static builds
  never compiled it — byte-identical removal.) **Kept**: `collect_partly` and the
  rest of `free_try_collect_mt` — they are the *core* cross-thread-free path
  (a free into a page abandoned at thread exit), not eviction.
- `theap.rs` (vestigial `type Theap = Heap`).

### Tests & benchmarks — keep only the peak-performance measurement
Keep only what measures **mimalloc-rs at its peak** (native, static global
allocator); archive the rest (system/C comparisons, preload, overlapping perf
harnesses) to `export`.
- **Keep**: `examples/stress.rs` (the official `test-stress` workload at peak),
  `benches/alloc.rs` (Criterion, native engine across size classes),
  `examples/global_allocator.rs` (canonical usage demo).
- **Keep (correctness, not perf)**: inline `#[cfg(test)]` unit tests,
  `tests/differential.rs` (vs C oracle), `tests/invariants.rs`, loom/TSan/Miri.
- **Archive**: `examples/{bench,bench_suite,microbench,profile_alloc,rss_spike}.rs`,
  `tests/preload.rs`, `scripts/{perf_compare,mimalloc-bench,preload-check}.sh`.

## 5. Execution (ordered; each phase its own PR, no-regression gate)

- **P0 — Archive.** Push current HEAD as `export`.
- **P1 — Remove eviction (FE2).** Make the plain `find_free_page` unconditional;
  drop `page_to_full` / `reclaim_on_free` / `PageFullRetain` + the reclaim step.
  Keep cross-thread free + abandoned reclaim (incl. `collect_partly`). Verify
  static path byte-identical.
- **P2 — Remove export surface.** Delete `capi` / `override` / `sysalloc` +
  features; re-point the `differential` harness to drive rs via the Rust API.
- **P3 — Minimize `unsafe` (the headline pass).** Encapsulate behind safe
  abstractions; reference/checked access where sound; measure unsafe count
  before/after. **No perf regression; Miri green.**
- **P4 — Comment trim + API polish.** Strip internal-logic prose; keep only
  Rust-deviation notes + `SAFETY`. Tighten the public API.
- **P5 — Docs + lock.** Rewrite `README` / `lib.rs` / `FIDELITY.md` for the
  rust-native scope; update memory.

## 6. Absolute gates (every PR)

- **Zero benchmark regression** vs `main`, measured on the pinned i7-14700K
  (the archived `perf_compare.sh`/`bench_suite` are replaced by these):
  - `examples/stress.rs` built on **both** the branch and `main`, interleaved
    median across thread counts — no regression (this is the branch-vs-`main`
    static gate now),
  - `cargo bench --bench alloc` (Criterion, no regression),
  - sanity vs the same-built C `test-stress` stays unchanged.
- Correctness all green: `cargo test` (+ secure/debug/stats/track), differential
  vs C v3.3.2, loom, TSan (`-Zbuild-std`), Miri (strict-provenance).
- `cargo fmt --all` + `clippy -D warnings` before every push.
- For P3: a reported **unsafe-count delta** (down), with the perf + Miri evidence.
