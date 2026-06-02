<!-- SPDX-License-Identifier: MIT -->
# Changelog

All notable changes to mimalloc-rs are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project is
pre-1.0 and not yet published to crates.io.

## [Unreleased]

### Added
- From-scratch Rust port of the **mimalloc v3** allocator core (`MI_MALLOC_VERSION
  30302`): segment-less arenas + atomic binned bitmap, lazy 2-level page-map,
  free-list sharding with the `xthread_free` MPSC protocol, per-thread heaps,
  and the bin/size-class model. `#![no_std]` core + `std` feature.
- Public surfaces: `GlobalAlloc` (`MiMalloc`), the stable `Allocator` trait
  (+ `core::alloc::Allocator` under `nightly`), the `mi_*` C-ABI (`capi`), and a
  transparent libc `override` (LD_PRELOAD) with foreign-pointer fallback.
- Security/diagnostics features: `secure` (encoded free lists + double/invalid-free
  detection), `debug` (padding/canaries), `stats`, `track`.
- Page lifecycle: retire empty pages to the arena; thread-exit page hand-off
  (abandon/reclaim).
- **Delayed purge + on-demand commit** — freed arena slices are returned to the
  OS (`MADV_DONTNEED`) after a delay (mimalloc's `purge_delay`/`arena_purge_mult`
  model), so RSS tracks the live set, not the high-water mark.
- Lazy page free-list extend (`capacity` vs `reserved`).
- Large/huge OS pages + NUMA awareness; full `MIMALLOC_*` option surface and
  per-bin/arena statistics.

### Verification & tooling
- Differential test vs the C `libmimalloc` (observable-contract oracle: all size
  bins, edge-case API, cross-thread), CI-gated against v3.3.2.
- Concurrency model-checking (loom), Miri (strict-provenance), AddressSanitizer
  + ThreadSanitizer CI, cargo-fuzz harness, coverage.
- Benchmarks: Criterion micro-benchmarks, `bench_suite`/`microbench`/`rss_spike`
  examples, the `perf_compare.sh` no-regression harness, and a `mimalloc-bench`
  runner. See `docs/benchmarking.md` and `docs/verification.md`.

### Platform
- Linux (x86-64) only for now; Windows/macOS `Prim` backends are follow-up work.
