<!-- SPDX-License-Identifier: MIT -->
# Fidelity: mimalloc v3 port vs. Rust-idiomatic deviations + honest benchmarks

mimalloc-rs is a from-scratch Rust re-implementation of mimalloc **v3.3.2**
(`MI_MALLOC_VERSION 30302`), not an FFI binding. This document records, for an
upstream-contribution audience:

1. which v3 mechanisms are ported **faithfully** (with C citations),
2. where the port deliberately uses **Rust strengths** instead, and where it
   **diverges** from v3 and why,
3. **honest** performance numbers against the C reference under a *fair*
   comparison, including the workloads where we still trail.

## 1. Faithfully ported v3 mechanisms

| Mechanism | v3 source | mimalloc-rs |
|---|---|---|
| Segment-less arenas: 64 KiB slices + atomic binned bitmaps | `arena.c`, `bitmap.c` | `arena.rs`, `bitmap.rs` |
| 2-level page-map (address → page) | `page-map.c` | `page_map.rs` |
| Free-list sharding: `free` / `local_free` / `xthread_free` (MPSC) | `page.c`, `free.c` | `page.rs` |
| Flag-folded free fast path: `xtid = tid ^ xthread_id`, flags in low 2 bits | `free.c:185-205` (`mi_free_ex`) | `heap.rs::free` |
| `xthread_free` ownership bit (`block | owned`), claim = `fetch_or(1)` | `internal.h` `mi_tf_*`, `mi_page_claim_ownership` | `page.rs` `tf_*` / `claim_ownership` |
| `_mi_page_free_collect`: O(1) `free = local_free` head move | `page.c:206-236` | `page.rs::collect` (the O(1) splice) |
| Abandoned-page reclaim via per-arena `pages_abandoned[bin]` bitmap | `arena.c` | `arena.rs`, `subproc.rs` |
| Cross-thread free → claim → free/reclaim/reabandon/unown | `free.c` `mi_free_try_collect_mt` | `heap.rs::free_try_collect_mt` |
| Lazy page extend (capacity vs reserved), bounded batch | `page.c` `mi_page_extend_free` | `page.rs::extend_free` |
| Delayed purge (`MADV_DONTNEED`) with per-arena purge bitmap + expiry | `arena.c` purge | `arena.rs`, `os.rs::purge_ex` |
| Size→bin mapping, `MI_BIN_HUGE=73`/`FULL=74`/`COUNT=75`, `MI_SMALL_MAX_OBJ_SIZE=10240` | `types.h`, `page-queue.c` | `bits.rs`, `page_queue.rs` |
| Option table + `MIMALLOC_*` env, v3 defaults (`purge_delay=1000`, …) | `options.c` | `options.rs` |
| Secure/debug hardening: invalid/double-free detection, abort gate | `free.c` | `heap.rs` (`#[cfg(secure/debug)]`) |
| TLS model = initial-exec for the preloaded library (`MI_TLS_MODEL`) | compiler attr | preload build recipe (`-Z tls-model=initial-exec`) |

Correctness is locked against the C original by a **differential harness**
(`tests/differential.rs`, run vs C v3.3.2 in CI) plus loom, Miri
(strict-provenance), TSan, and the secure/debug abort gate.

## 2. Rust-idiomatic choices (using the language's strengths)

- **Errors & nullability**: `Option`/`Result`/`NonNull<u8>` instead of NULL +
  errno. OOM is `None`, never UB.
- **RAII**: `Heap`/`Arena`/`OsMem` `Drop` return memory and hand off pages on
  thread exit, rather than manual teardown calls.
- **Encapsulated `unsafe`**: pointer/atomic/page-map/free-list cores carry a
  `// SAFETY:` justification per block; the hot path is panic-free (no
  `unwrap`/`expect`, OOM → `None`, invariant violations → `abort`, never unwind
  across the `mi_*` boundary).
- **Strict provenance**: free-list encoding and `xthread_free` ownership tagging
  use `map_addr`/`with_addr` (provenance-preserving, no expose round-trip);
  encoded pointers use `expose_provenance`/`with_exposed_provenance`. Gated by
  Miri `-Zmiri-strict-provenance`.
- **Typed intent**: `Cell` for owner-only mutable fields, `AtomicPtr` for the
  cross-thread head (provenance-preserving), newtypes for thread-free words.
- **Compile-time tables**: the bin→block-size table is a `const` (was a runtime
  `OnceBox`), matching C's compile-time `pages[bin].block_size` with no init
  atomic — a Rust `const fn` win over C's macro tables.

### A Rust-specific codegen deviation: `cfg`-gated fast-path shape
C force-inlines the alloc/free fast path (`mi_decl_forceinline`) over a noinline
generic path (`_mi_malloc_generic`), because every C build crosses a call into
the library. A Rust `#[global_allocator]` is **statically linked** and the
compiler inlines the whole chain holistically — there, forcing the slow path out
of line *regresses* the small-alloc hot path. So the split's `#[cold]` marker is
applied **only in the preload `cdylib`** (`#[cfg_attr(override_export, cold)]`):
each link model gets its optimal codegen. (`#[inline]` is ignored on `#[no_mangle]`
exports, so the inlining is driven by the internal shells folding into the
exported body — see `docs/perf-hotpath.md` §C2/§C3.)

## 3. Deliberate divergences from v3 (and why)

| Divergence | v3 behavior | mimalloc-rs | Rationale / status |
|---|---|---|---|
| **Full-page eviction** | full pages are abandoned out of the bin queue (`page_full_retain`) so cross-thread freers can claim them | full pages stay in the bin queue (FE2 **parked**) | Two attempts regressed larson/phase-2 MT. **This is the main remaining gap** — see §4. |
| **Delayed retire** | `retire_expire` countdown; emptied sole pages freed later by `_mi_theap_collect_retired` on a generic-alloc cadence | emptied pages retired immediately; the sole page of a bin is kept | Keeping the sole page covers the common alloc/free/alloc cycle; the cadence + `retire_expire` is unported (also breaks the deferred-free heartbeat contract — follow-up). |
| **Reclaim-on-free** | a cross-thread free can reclaim an abandoned page into the freeing thread (`page_reclaim_on_free`, `max_reclaim` cap) | deferred: a freed-into page is reabandoned-to-mapped and reclaimed on the next *alloc* instead | Avoids free↔heap coupling; costs contention on heavy cross-thread frees (§4). |
| **`_mi_page_free_collect_partly`** | no-atomic small-block collect on the claim path | always full-collects (atomic swap-drain) | Only on the abandoned-claim path; matters once eviction (above) is in play. |
| **TLS storage** | `__thread mi_heap_t*` pointer; heap allocated out of line | whole `Heap` stored inline in TLS | Faster for the dominant static `#[global_allocator]` use (direct TLS address, no deref); makes `dlopen`+initial-exec need a larger static TLS block (documented). |
| **subproc / NUMA / Windows·macOS** | full multi-subproc, NUMA, all platforms | single main subproc, Linux-first | Scoped for v1; follow-up. |

## 4. Honest fair-benchmark results

**Methodology.** The only fair comparison loads *both* allocators the same way —
as `LD_PRELOAD`-ed shared objects (driving mimalloc-C via `dlopen` function
pointers while inlining mimalloc-rs flatters the latter). All numbers are on a
pinned, quiet i7-14700K, cores 2–9, interleaved median (which cancels machine
drift — a non-interleaved run can swing a throughput bench by ±8%). rs is built
`--features override` as a `cdylib` with `-Z tls-model=initial-exec`.

vs C `libmimalloc` v3.3.2 (− = rs slower / costlier):

| workload | kind | gap to C | note |
|---|---|---|---|
| espresso | 1T | **−1.8%** | |
| malloc-large | 1T | **−2.4%** | |
| larson | 8T | **−2.6%** | thread-respawn cross-thread |
| mstress | 8T | **≈0%** | |
| rptest | 8T | **≈0%** | |
| cfrac | 1T | **−4.0%** | pure malloc/free |
| **alloc-test** | 8T | **+14.6% slower** | heavy cross-thread free |
| **xmalloc-test** | 8T | **−24% throughput** | producer/consumer cross-thread free |

(All trail or match; rs beats glibc on every workload — e.g. xmalloc-test rs
232 M vs glibc 63 M free/sec.)

### The remaining gap is concentrated in heavy cross-thread free

`xmalloc-test` and `alloc-test` are producer/consumer stresses: some threads
allocate, *other* threads free. Profiling pins the cause precisely:

- **mimalloc-rs**: 46% of time is in `free`, and **51% of that is a single
  `lock cmpxchg` on `xthread_free`** (plus `pause` spin-retries) — every
  cross-thread free atomically pushes onto the *live owner's* page, and many
  consumer threads contend on the same page's head. The owner's alloc then keeps
  hitting the cold collect/generic path (21% `alloc_generic`).
- **mimalloc-C**: `mi_free_try_collect_mt` (31%) + `_mi_page_free_collect_partly`
  (4.5%) + `_mi_arenas_page_try_reabandon_to_mapped` (5%) +
  `mi_abandoned_page_try_reclaim` (2.9%). C **abandons** the page so a freeing
  thread **claims** it and collects **locally without atomics** (`_partly`),
  sidestepping the contended CAS.

So the root cause is the trio mimalloc-rs does not yet have: **full-page
eviction/abandonment (FE2, parked) → reclaim-on-free → no-atomic `_partly`
collect**. Closing it is a substantial, regression-prone effort (FE2 has been
parked twice for regressing larson/phase-2) and is tracked as future work, not
forced through under the no-regression bar. For workloads that are not
dominated by cross-thread frees, mimalloc-rs is within ~4% of the C reference.

## 5. Summary
The core v3 design — segment-less arenas, free-list sharding, the flag-folded
free fast path, ownership-tagged cross-thread frees, the O(1) collect, delayed
purge — is ported faithfully and verified differentially against C v3.3.2. The
port leans on Rust's type system, RAII, strict provenance, and compile-time
evaluation where they are strict improvements, and documents each divergence.
On a fair LD_PRELOAD comparison it matches the C reference to within a few
percent on most workloads; the open gap is heavy cross-thread-free contention,
whose fix (reviving full-page eviction + reclaim-on-free + `_partly` collect) is
identified and deferred rather than rushed.
