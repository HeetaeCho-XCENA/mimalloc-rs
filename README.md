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

As a **C library** (`capi` feature): build a `cdylib`/`staticlib` exporting the
mimalloc-compatible C ABI (`mi_malloc`/`mi_free`/`mi_calloc`/`mi_realloc`/aligned
variants/`mi_posix_memalign`/`mi_new*`/`mi_heap_*`/`mi_option_*`/`mi_stats_*`/…):

```sh
cargo rustc --release --features capi --crate-type cdylib   # → libmimalloc_rs.so
```

First-class heaps from Rust: `Heap::new_boxed`, `Heap::{delete,destroy}`.

As a **transparent `LD_PRELOAD` drop-in** (`override` feature): also export the
standard libc symbols (`malloc`/`free`/`calloc`/`realloc`/`aligned_alloc`/
`posix_memalign`/`valloc`/…) so an unmodified program uses this allocator. The
raw symbols are emitted only when the explicit `override_export` cfg is set (so
they never leak into `cargo test`/`build`):

```sh
RUSTFLAGS="--cfg override_export" \
  cargo rustc --release --features override --crate-type cdylib   # → libmimalloc_rs.so
LD_PRELOAD=./target/release/libmimalloc_rs.so  ./your_program
```

Pointers allocated before interposition (or by paths we don't intercept) are
detected by arena-membership and forwarded to the real system allocator, so
mixing is safe.

#### Verifying

`scripts/preload-check.sh` is the end-to-end proof: it builds the override
cdylib (into an isolated `target/preload`), compiles an unmodified C probe, and
asserts that under `LD_PRELOAD` **every** `malloc` is served by this allocator
(`mi_is_in_heap_region(p) == true` for all of them — `ours == total`). It also
smoke-tests a real system binary (`/bin/ls`) under preload to confirm
transparent replacement doesn't crash it. It exits non-zero on any failure, and
is a clean no-op (exit 0, "SKIP") on machines without a C compiler.

```sh
bash scripts/preload-check.sh   # → PRELOAD CHECK: PASS
```

The `preload` CI job runs this on every push, so transparent override is
validated continuously. (A `#[ignore]`d `tests/preload.rs` wraps the same
script for local convenience — run with
`cargo test --features override --test preload -- --ignored`.)

## Features

| feature   | default | effect                                                        |
|-----------|:-------:|---------------------------------------------------------------|
| `std`     |   ✅    | OS threads + `thread_local!` default heap; `GlobalAlloc`/`Allocator` |
| `nightly` |         | also impl `core::alloc::Allocator`; `#[thread_local]` fast path |
| `secure`  |         | encoded free lists + hardening (`MI_SECURE`)                  |
| `debug`   |         | padding/canaries + extra checks (`MI_PADDING`/`MI_DEBUG`)     |
| `stats`   |         | process-wide allocation counters (`mi_stats_*`)              |
| `track`   |         | Valgrind/ASan tracking hooks                                  |
| `capi`    |         | export the C-ABI `mi_*` symbols (`#[no_mangle] extern "C"`)   |
| `differential` |    | enable the `libmimalloc` FFI differential test (see below)   |

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

## License

MIT, matching upstream mimalloc. The original C implementation is
Copyright © Microsoft Research, Daan Leijen.
