<!-- SPDX-License-Identifier: MIT -->
# Engine fidelity: mimalloc v3 ↔ mimalloc-rs

mimalloc-rs is a from-scratch Rust re-implementation of the mimalloc **v3.3.2**
engine (`MI_MALLOC_VERSION 30302`), built to run as a Rust-native static
`#[global_allocator]`. This document records, for someone who knows mimalloc:

1. which v3 mechanisms are ported **faithfully** (with C citations), and
2. where the port deliberately uses a **Rust-idiomatic** form instead, and why.

Benchmark numbers live in [`BENCHMARKS.md`](BENCHMARKS.md). Correctness is locked
against the C original by a **differential harness** (`tests/differential.rs`, run
vs C v3.3.2 in CI) plus loom, Miri (strict-provenance), and TSan.

## 1. Faithfully ported v3 mechanisms

| Mechanism | v3 source | mimalloc-rs |
|---|---|---|
| Segment-less arenas: 64 KiB slices + atomic binned bitmaps | `arena.c`, `bitmap.c` | `arena.rs`, `bitmap.rs` |
| 2-level page-map (address → page) | `page-map.c` | `page_map.rs` |
| Free-list sharding: `free` / `local_free` / `xthread_free` (MPSC) | `page.c`, `free.c` | `page.rs` |
| Flag-folded free fast path: `xtid = tid ^ xthread_id`, flags in low 2 bits | `free.c:185-205` (`mi_free_ex`) | `heap.rs::free` |
| `xthread_free` ownership bit, claim = `fetch_or(1)` | `internal.h` `mi_tf_*`, `mi_page_claim_ownership` | `page.rs` `tf_*` |
| `_mi_page_free_collect`: O(1) `free = local_free` head move | `page.c:206-236` | `page.rs::collect` |
| No-atomic small-block claim collect (`_mi_page_free_collect_partly`) | `free.c:243` | `page.rs::collect_partly` |
| Abandoned-page reclaim via per-arena `pages_abandoned[bin]` bitmap | `arena.c` | `arena.rs`, `subproc.rs` |
| Cross-thread free → claim → free / reabandon / unown | `free.c` `mi_free_try_collect_mt` | `heap.rs::free_try_collect_mt` |
| Lazy page extend (capacity vs reserved), bounded batch | `page.c` `mi_page_extend_free` | `page.rs::extend_free` |
| Delayed purge (`MADV_DONTNEED`) with per-arena purge bitmap + expiry | `arena.c` purge | `arena.rs`, `os.rs::purge_ex` |
| Delayed retire: `retire_expire` countdown on emptied sole pages + `collect_retired` cadence | `page.c:422-496` | `page.rs` `retire_expire`, `heap.rs::collect_retired` |
| `mi_heap_t` / `mi_theap_t` split: logical heap vs thread-local execution; `page->theap`; per-heap theaps list | `types.h:504-577`, `page.c:696`, `theap.c` | `heap.rs` `Heap` / `ThreadHeap`, `Page.theap` |
| True shared first-class heap: per-thread theap via `mi_heap_get_theap`, refcounted lifecycle | `theap.c`, `types.h:507` | `heap.rs` `theap_for`/`refcount`, `init.rs` registry |
| Size→bin mapping, `MI_BIN_HUGE=73`/`FULL=74`/`COUNT=75`, `MI_SMALL_MAX_OBJ_SIZE=10240` | `types.h`, `page-queue.c` | `bits.rs`, `page_queue.rs` |
| Option table + `MIMALLOC_*` env, v3 defaults (`purge_delay=1000`, …) | `options.c` | `options.rs` |
| Secure/debug hardening: invalid/double-free detection, abort gate | `free.c` | `heap.rs` (`#[cfg(secure/debug)]`) |
| Large/huge OS pages + NUMA-aware arena placement | `os.c`, `prim` | `os.rs`, `prim/linux.rs` |

## 2. Rust-idiomatic choices (using the language's strengths)

- **Errors & nullability**: `Option`/`Result`/`NonNull<u8>` instead of NULL +
  errno. OOM is `None`, never UB.
- **RAII**: `Heap`/`Arena`/`OsMem` `Drop` return memory and hand off pages on
  thread exit, instead of manual teardown calls.
- **Encapsulated `unsafe`**: the irreducible `unsafe` (owner-exclusive `Cell`
  fields, FFI, free-list/page-map pointer encoding, atomics over OS-backed
  storage) is confined to its modules; each block carries a `// SAFETY:`
  justification; the hot path is panic-free (OOM → `None`, invariant violations
  → `abort`, never unwind across the allocator boundary). See the unsafe-policy
  section of the crate docs (`lib.rs`).
- **Strict provenance**: free-list encoding and `xthread_free` ownership tagging
  use `map_addr`/`with_addr` (no expose round-trip); encoded pointers use
  `expose_provenance`/`with_exposed_provenance`. Gated by Miri
  `-Zmiri-strict-provenance`.
- **Typed intent**: `Cell` for owner-only mutable fields, `AtomicPtr` for the
  cross-thread head, newtypes for thread-free words.
- **Compile-time tables**: the bin→block-size table is a `const` (vs C's macro
  tables), with no init atomic.
- **Composable + extensible**: `MiMalloc` implements both `GlobalAlloc` (the
  primary use) and `Allocator` (per-container, stable `allocator-api2` + nightly
  `core::alloc`); the engine is `#![no_std]`-capable with a `Prim` OS-backend
  trait.

## 3. Deliberate divergences from v3 (and why)

| Divergence | v3 behavior | mimalloc-rs | Rationale |
|---|---|---|---|
| **Full-page eviction** | full pages are abandoned out of the bin queue (`page_full_retain`) so cross-thread freers can claim them | not ported | It only paid off for the `LD_PRELOAD`/C-replacement build (it regresses the single/intra-thread small-alloc path with no contention to relieve); that build lives on the `export` branch. Thread-exit abandon + reclaim-on-alloc remain. |
| **TLS storage** | `__thread mi_theap_t*` pointer; theap allocated out of line | whole default `ThreadHeap` stored inline in TLS | Faster for the static `#[global_allocator]` (direct TLS address, no deref); an out-of-line pointer cache was measured and showed no win. |
| **`mi_heap_delete`/`destroy` reclaim scope** | walk the heap's whole `theaps` list and reclaim every thread's theap, arbitrating against concurrent thread-exit via a `theap->heap` exchange | reclaim the **calling thread's** theap eagerly; theaps other threads hold are reclaimed when those threads exit. A refcount frees the heap only once the handle **and** the last theap are gone (no use-after-free; the heap outlives every theap). | Safe and complete for the common cases (a first-class heap used by one thread, or a shared heap freed at process exit). Eager full-list teardown with the `_mi_theap_free` CAS-claim arbitration is a follow-up; until then a long-lived thread sharing a destroyed heap defers its page reclaim to its own exit. |
| **subproc / NUMA / Windows·macOS** | full multi-subproc, all platforms | single main subproc, Linux-first | Scoped for v1; follow-up. |

## 4. Summary

The core v3 design — segment-less arenas, free-list sharding, the flag-folded
free fast path, ownership-tagged cross-thread frees, the O(1) collect, delayed
purge, delayed retire (`retire_expire` + `collect_retired` cadence), the
`mi_heap_t` / `mi_theap_t` split with true cross-thread shared first-class heaps,
and abandoned-page reclaim — is ported faithfully and verified
differentially against C v3.3.2. The port leans on Rust's type system, RAII,
strict provenance, and compile-time evaluation where they are strict
improvements, and documents each divergence. Built as a Rust-native static
allocator it is on par with mimalloc-c at peak (see [`BENCHMARKS.md`](BENCHMARKS.md)).
