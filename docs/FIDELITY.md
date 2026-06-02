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
| **Full-page eviction** | full pages are abandoned out of the bin queue (`page_full_retain`) so cross-thread freers can claim them | implemented **for the preload `cdylib` only** (`cfg(any(override_export, test))`); a static `#[global_allocator]` keeps full pages in the bin queue | Ported with cross-thread reclaim + a tuned `page_full_retain=16` (FR2/FR3). Closes the cross-thread-free gap (xmalloc-test −24%→−11%, §4). Gated to preload because the abandon/reclaim churn regresses the single/intra-thread small-alloc path with no contention to relieve; a static build is byte-identical to the pre-eviction scan. (Bare eviction was parked 4× before — cross-thread reclaim is what makes it net-positive.) |
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
| **xmalloc-test** | 8T | **−10.8%** (was −24%) | producer/consumer cross-thread free — closed by FR2 (below) |
| **alloc-test** | 8T | **+15% slower** | heavy cross-thread free; not improved by eviction (different bottleneck) |

(All trail or match; rs beats glibc on every workload — e.g. xmalloc-test rs
274 M vs glibc 63 M free/sec.)

### The heavy-cross-thread-free gap, and how it was closed (FR2/FR3)

`xmalloc-test` and `alloc-test` are producer/consumer stresses: some threads
allocate, *other* threads free. Profiling pinned the cause: every cross-thread
free atomically pushes onto the *live owner's* page (`free` was 46% of time, 51%
of that one `lock cmpxchg` + `pause` spin), with many consumers contending on the
same head. mimalloc-C avoids it by **abandoning** full pages so a freeing thread
**claims** one and collects locally; crucially it then keeps the page (reclaim),
distributing pages across the freeing threads → uncontended local ops.

The fix (round `docs/GOAL-fe2-revival.md`, merged): **full-page eviction +
cross-thread reclaim of mostly-free pages + the no-atomic `_mi_page_free_collect_partly`**.
Together they took xmalloc-test from **−24% → −10.8%** with no regression on
larson/cfrac/espresso/mstress/rptest/malloc-large. Bare eviction had been parked
**4×** (it turns a 1-CAS push into a 3-CAS claim/reabandon/unown cycle); the
piece that makes it net-positive is **cross-thread reclaim** — handing the page
to the freeing thread so its subsequent frees are local. Because that
abandon/reclaim churn *regresses* the single/intra-thread small-alloc path (no
contention to relieve), it is enabled **only in the preload `cdylib`**
(`cfg(any(override_export, test))`, runtime `page_full_retain=16`); a static
`#[global_allocator]` keeps the byte-identical pre-eviction scan.

`alloc-test` (+15%) is *not* helped by eviction — its bottleneck is elsewhere
(left as future work).

## 5. Summary
The core v3 design — segment-less arenas, free-list sharding, the flag-folded
free fast path, ownership-tagged cross-thread frees, the O(1) collect, delayed
purge, and (preload) full-page eviction with cross-thread reclaim — is ported
faithfully and verified differentially against C v3.3.2. The port leans on Rust's
type system, RAII, strict provenance, compile-time evaluation, and a
static-vs-preload `cfg` split where they are strict improvements, and documents
each divergence. On a fair LD_PRELOAD comparison it is within ~2–11% of the C
reference across the suite; the one remaining outlier is `alloc-test` (+15%),
identified as a non-eviction bottleneck and left for future work.
