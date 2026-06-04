<!-- SPDX-License-Identifier: MIT -->
# mimalloc-rs

A from-scratch Rust re-implementation of the **mimalloc v3** general-purpose
allocator (`MI_MALLOC_VERSION 30302`). This is **not** an FFI binding to the C
library — the allocation engine is ported to idiomatic, `#![no_std]`-capable
Rust:

- **Free-list sharding** with `free` / `local_free` / `xthread_free` lists.
- **Segment-less v3 design**: arenas carve 64 KiB slices directly into pages via
  an atomic binned bitmap; an O(1) two-level **page-map** maps addresses back to
  pages.
- **Lock-free cross-thread free** through an atomic Treiber stack (`xthread_free`),
  proven with `loom`.
- **Security options**: encoded free lists (XOR/rotate with random keys) under
  the `secure`/`debug` features.

The OS layer (Linux-first: `mmap`/`munmap`/`mprotect`/`madvise`) sits behind a
`Prim` trait; Windows/macOS are planned follow-ups.

## Status

This is an in-progress port. Implemented and tested: constants/size-classes, the
OS/prim layer, the atomic bitmap, arenas + metadata allocator + page-map, pages
& free lists, heaps, thread-local default heap, **end-to-end `malloc`/`free`**,
cross-thread free, and the public API (`GlobalAlloc`, `Allocator`, `mi_*`).

Known follow-ups: abandoned-page reclaim on thread exit (`pthread_key` teardown),
guarded-sampling allocations, the full options/stats tables, and the Windows/macOS
`Prim` backends.

## Usage

As the global allocator:

```rust
use mimalloc_rs::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
```

With the `Allocator` API (stable via `allocator-api2`, or nightly `core`):

```rust
use mimalloc_rs::MiMalloc;
use allocator_api2::boxed::Box;

let b = Box::new_in(42u64, MiMalloc);
```

First-class heaps from Rust: `Heap::new_boxed`, `Heap::{delete,destroy}`.

> **C ABI / `LD_PRELOAD` drop-in.** mimalloc-rs is a *rust-native* engine — its
> intended use is the static `#[global_allocator]` above, where it is on par with
> mimalloc-c (see [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md)). The C-ABI (`mi_*`)
> and transparent libc / `LD_PRELOAD` override build (which pays an unavoidable
> `.so`-boundary cost) live on the **`export`** branch.

## Features

| feature   | default | effect                                                        |
|-----------|:-------:|---------------------------------------------------------------|
| `std`     |   ✅    | OS threads + `thread_local!` default heap; `GlobalAlloc`/`Allocator` |
| `nightly` |         | also impl `core::alloc::Allocator`; `#[thread_local]` fast path |
| `secure`  |         | encoded free lists + hardening (`MI_SECURE`)                  |
| `debug`   |         | padding/canaries + extra checks (`MI_PADDING`/`MI_DEBUG`)     |
| `stats`   |         | process-wide allocation counters (`mi_stats_*`)              |
| `track`   |         | Valgrind/ASan tracking hooks                                  |
| `differential` |    | enable the `libmimalloc` FFI differential test (`MIMALLOC_C_LIB`) |

## Building & testing

```sh
cargo build                       # std (default)
cargo build --no-default-features # no_std core (compile + TLS-free unit tests)
cargo clippy --all-targets -- -D warnings
cargo test                        # unit + integration (incl. randomized invariants)
RUSTFLAGS="--cfg loom" cargo test --lib loom   # concurrency models (bitmap CAS, xthread_free MPSC)
cargo +nightly miri test --lib bitmap          # UB/provenance on pure logic
cargo run --example global_allocator
```

### Differential verification

`tests/invariants.rs` runs a long deterministic pseudo-random workload of
`alloc`/`alloc_aligned`/`zalloc`/`realloc`/`free` and asserts the observable
invariants any correct allocator must hold (alignment, zeroing, usable-size,
**no overlap of live allocations**, realloc content preservation, no leak). This
invariant oracle is exactly what a side-by-side comparison against the C original
would check.

`tests/differential.rs` (the `differential` feature) links the **C
`libmimalloc`** and runs the same workload through both the C reference and
mimalloc-rs, asserting they uphold the identical observable contract and report
the same version:

```sh
MIMALLOC_C_LIB=/path/to/mimalloc/out cargo test --features differential --test differential
```

## Contributing

Contributions are welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) for dev
setup (MSRV 1.84), the gate checklist, the `unsafe` policy, and the no-regression
rule. Reference docs:

- [`docs/FIDELITY.md`](docs/FIDELITY.md) — engine compatibility with mimalloc v3:
  which mechanisms are ported faithfully and where the port is Rust-idiomatic.
- [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) — peak-vs-peak results vs mimalloc-c
  and how to reproduce.

Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

MIT, matching upstream mimalloc. The original C implementation is
Copyright © Microsoft Research, Daan Leijen.
