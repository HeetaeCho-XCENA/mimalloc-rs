<!-- SPDX-License-Identifier: MIT -->
# Porting status: mimalloc-c v3.3.2 ↔ mimalloc-rs

A module-by-module survey of how faithfully mimalloc-rs ports mimalloc **v3.3.2**
(`MI_MALLOC_VERSION 30302`) and where it deliberately re-creates things in
idiomatic Rust. Companion to `docs/FIDELITY.md` (which focuses on the
benchmark-driven perf round) — this file is the **structural parity reference**.

Status legend:

- ✅ **Faithful** — observable behavior matches C; same constants/protocol.
- 🦀 **Rust-recreated** — same behavior, but the *implementation* uses a Rust
  idiom that is a strict improvement (documented in §3).
- 🟡 **Partial** — core ported, some sub-features/tuning unported.
- 🔵 **Deferred / divergent** — intentionally not matched (see §5 + FIDELITY §3).

> Line counts are a rough scope signal, not a fidelity metric: Rust expresses
> some C in far fewer lines (and vice-versa). C total ≈ 15.8k LOC across `src/`;
> rs core ≈ 9.2k LOC across `src/` (excluding the ~1.8k-line `capi.rs` C-ABI
> surface which has no single C counterpart).

## 1. Module map (C source → Rust module)

| C source (LOC) | Rust module (LOC) | Status | Notes |
|---|---|---|---|
| `arena.c` (2514) | `arena.rs` (685) + `subproc.rs` (234) | ✅🟡 | Slice-bitmap alloc, page carve, `pages_abandoned[bin]` registry, abandon/reclaim/unabandon. **Partial:** no per-bin chunkmap size-class binning (single conservative chunkmap skip); reclaim uses ownership-claim instead of the reader busy-wait handshake (🦀, §3). |
| `bitmap.c` (1949) | `bitmap.rs` (838) | ✅🟡🦀 | Binned atomic bitmap: find-and-set/clear, `try_find_and_clear_n`, CAS loops, chunkmap. **🦀** `try_find_and_claim` clears the bit only after the ownership callback succeeds (removes C's clear→reset churn + `clear_once_set` handshake). **Partial:** the reclaim scan is a per-bit loop, not a `trailing_zeros`/SIMD bit-scan. |
| `init.c` (1199) | `init.rs` (512) | ✅🟡 | `process_init` once + reentrant lazy init, thread init/done, static bootstrap, TLS default heap, `mi_register_deferred_free`. **Partial:** the generic-alloc *cadence* (`generic_count`→periodic `collect_retired`/deferred-free every ~1000/~10000 allocs) is not wired. |
| `page.c` (1039) | `page.rs` (1032) | ✅🔵 | Page state machine, three free lists, `_mi_page_free_collect` **O(1) splice**, `extend_free` (lazy/bounded), `xthread_free` ownership tags, retire. **Deferred:** `retire_expire` delayed retire, `mi_page_queue_find_free_ex` candidate search, `mi_page_to_full` eviction (§5). |
| `os.c` (923) | `os.rs` (402) | ✅🟡 | reserve vs commit, decommit/reset/`purge_ex` (decommit vs reset + needs-recommit), `is_zero`. Huge-page/NUMA hooks present via `prim`. **Partial:** narrower huge/NUMA surface than C. |
| `alloc.c` (885) | `heap.rs` (alloc core) + `capi.rs` | ✅ | malloc fast path (`pages_free_direct`) vs generic, calloc/realloc, zeroing. Fast/slow split mirrors `mi_decl_forceinline`/`_mi_malloc_generic` (🦀 cfg-gated, §3). |
| `free.c` (658) | `heap.rs::free` + `page.rs` | ✅🔵 | Flag-folded XOR dispatch (`xtid = tid ^ xthread_id`), local push, cross-thread atomic push, claim→collect. **Deferred:** `_mi_page_free_collect_partly` (no-atomic small collect) and reclaim-on-free into the originating heap (§5). |
| `theap.c` (715) + `threadlocal.c` (236) | `theap.rs` (16, alias) + `init.rs` TLS | 🔵 | **No `mi_theap_t`/`tld` split:** `Heap` doubles as the thread heap, stored inline in TLS. Heartbeat + thread-exit page handoff live on `Heap`. The dedicated theap/tld struct is future work. |
| `stats.c` (813) | `stats.rs` (138) | 🟡 | Process-level counters (alloc/free/current/peak/pages) behind `stats`. **Partial:** not the full per-bin/per-arena table C emits. |
| `options.c` (704) | `options.rs` (223) | 🟡 | Env-seeded, runtime-settable atomics backing `mi_option_*`. **Partial:** a representative subset (~7: verbose, eager_commit, purge_decommits, purge_delay, arena_reserve, arena_purge_mult, …) is wired to behavior; the full ~30-entry `mi_option_t` table is follow-up. |
| `page-map.c` (447) | `page_map.rs` (204) | ✅ | 2-level address→page map, acquire/release publish, strict-provenance restore. |
| `page-queue.c` (458) | `page_queue.rs` (149) | ✅🔵 | Per-bin intrusive queue (push_front/remove), `_mi_bin` mapping. **Deferred:** `MI_BIN_FULL` transitions + `mi_page_queue_move_to_front` (needed by the candidate search / eviction, §5). |
| `alloc-aligned.c` (441) | `heap.rs::alloc_aligned` | ✅🟡 | Over-aligned alloc via over-allocation + interior-pointer flag. **Partial:** no natural-alignment fast path (a power-of-two small alloc could skip the over-alloc). |
| `alloc-override.c` (396) | `override_symbols.rs` (190) | ✅🔵 | `#[no_mangle]` libc symbols (`malloc`/`free`/`calloc`/`posix_memalign`/…) for LD_PRELOAD. **Deferred:** C++ mangled `operator new`/`delete` (`_Znwm`/`_ZdlPv`) symbols. |
| `alloc-posix.c` (202) | `capi.rs` | ✅ | `posix_memalign`, `aligned_alloc`, `valloc`, `reallocarray`, … |
| `arena-meta.c` (179) | `arena_meta.rs` (197) | ✅ | Metadata arena (heap/page headers), recursion cycle broken by a static seed. |
| `random.c` (258) | `free_list.rs` keys + `prim` entropy | 🟡 | Free-list encoding keys (`rotl(addr ^ k1, k0) + k0`) + OS entropy via `prim`. **Partial:** no standalone `mi_random_t` Chacha PRNG stream; keys seeded from entropy. |
| `prim/unix/prim.c` (995) | `prim/linux.rs` + `prim/mod.rs` | ✅🟡 | `Prim` trait + Linux backend (mmap/munmap/mprotect/madvise, clock, getenv, thread-done key, NUMA/huge hooks). **Partial:** Linux-only (Windows/macOS deferred). |
| `libc.c` (477) | (std / `capi.rs`) | 🔵 | mimalloc ships its own libc-free helpers for freestanding builds; rs uses `core`/`std` equivalents. |
| `static.c` (43) | (n/a) | 🔵 | C single-TU amalgamation; not applicable to the Rust crate. |

## 2. Constants & invariants (re-derived from `types.h`/`mimalloc.h`)

✅ All size-class / layout constants are re-derived directly from the C headers
(not from a digest) and checked: `MI_ARENA_SLICE_SIZE=65536`,
`MI_SMALL_MAX_OBJ_SIZE=10240`, `MI_MEDIUM_MAX_OBJ_SIZE≈84KiB`,
`MI_LARGE_MAX_OBJ_SIZE=512KiB`, `MI_BIN_HUGE=73`/`MI_BIN_FULL=74`/`MI_BIN_COUNT=75`,
`MI_PAGE_FLAG_MASK=0x3`, `large-pages=1` pinned. The size→bin mapping is verified
against the C table in `bits.rs` tests. v3 option defaults match
(`purge_delay=1000`, `purge_decommits=1`, `arena_purge_mult=1`, `eager_commit=1`).

## 3. Rust-idiomatic recreations (same behavior, better implementation)

- **Errors/nullability** — `Option`/`Result`/`NonNull<u8>` instead of NULL+errno;
  OOM is `None`, never UB. RAII `Drop` (`Heap`/`Arena`/`OsMem`) replaces manual
  teardown and guarantees page handoff/slice return on scope exit.
- **Encapsulated `unsafe`** — atomic/pointer/page-map/free-list cores carry a
  `// SAFETY:` per block; the hot path is panic-free (no unwind across `mi_*`).
- **Strict provenance** — `xthread_free` ownership tagging and free-list encoding
  use provenance-preserving `map_addr`/`with_addr` (no expose round-trip);
  encoded/page-map integers use `expose_provenance`/`with_exposed_provenance`.
  Gated by Miri `-Zmiri-strict-provenance`.
- **Typed cross-thread word** — the `xthread_free` head is an `AtomicPtr<Block>`
  (provenance preserved) with `tf_block`/`tf_is_owned`/`tf_create` helpers, vs
  C's `uintptr_t` bit-twiddling.
- **Cell-typed owner state** — owner-only mutable page/heap fields are `Cell<_>`,
  encoding "single-owner, no atomics needed" in the type rather than convention.
- **Compile-time bin table** — `BIN_SIZES` is a `const` (a `const fn` loop),
  replacing a runtime `OnceBox` init atomic; matches C's compile-time
  `pages[bin].block_size` as a plain array read.
- **`cfg`-gated fast-path shape** — the alloc/free fast path is an `#[inline]`
  shell over a slow helper marked `#[cold]` **only in the preload cdylib**
  (`#[cfg_attr(override_export, cold)]`). C always crosses into the library, so it
  force-inlines unconditionally; a Rust static `#[global_allocator]` inlines the
  whole chain holistically and is *hurt* by forcing the split — so each link model
  gets its optimal codegen from one source. (See `docs/perf-hotpath.md` §C2/§C3.)
- **Ownership-gated reclaim (no reader handshake)** — `bitmap.rs::try_find_and_claim`
  claims page ownership first and clears the abandoned bit only on success,
  eliminating C's clear→reset-on-failure churn and the `mi_bitmap_clear_once_set`
  busy-wait; ownership is the single serialization point.

## 4. Public API & feature parity

| Surface | Status |
|---|---|
| `mi_*` C-ABI (malloc/free/calloc/realloc/aligned/usable_size/collect/heap-vars/…) | ✅ `capi.rs` (1829 LOC) |
| `GlobalAlloc` (`#[global_allocator]`) | ✅ `api/global_alloc.rs` |
| `Allocator` (allocator-api2 stable; `core` under `nightly`) | ✅ `api/allocator.rs` |
| C++ `new`/`delete` (`mi_new*`) | ✅ `capi.rs`; mangled `operator new/delete` symbols 🔵 deferred |
| LD_PRELOAD libc override | ✅ `override_symbols.rs` (`override` feature) |
| First-class heaps (`mi_heap_new`/`delete`/`destroy`) | ✅ `Heap::new_boxed`/`delete`/`destroy` |
| Features: `std`/`nightly`/`secure`/`debug`/`stats`/`track`/`capi`/`override`/`differential` | ✅ wired (`secure`/`stats`/`track` are minimal — §5) |
| Platforms | 🟡 Linux only (Windows/macOS prim deferred) |

## 5. Known divergences & deferred work (cross-ref FIDELITY §3)

- ✅🟡 **Full-page eviction** (`mi_page_to_full`/`page_full_retain`) — **ported for
  the preload `cdylib`** (`cfg(any(override_export, test))`, `page_full_retain=16`);
  a static `#[global_allocator]` keeps the pre-eviction first-fit scan
  (byte-identical) since the abandon/reclaim churn regresses the no-contention
  single/intra-thread path. With cross-thread reclaim + `_partly` (below) this
  closed the heavy-cross-thread-free gap (**xmalloc-test −24% → −11%**). Bare
  eviction had been parked 4× (`archive/v-round-v1v2`, `archive/fe2-full-page-eviction`).
- ✅ **Reclaim-on-free** (`mi_abandoned_page_try_reclaim`) — a thread claiming an
  abandoned page reclaims it into its own heap when originating **or** not
  mostly-used (cross-thread reclaim of mostly-free pages). This is the piece that
  makes eviction net-positive (distributes pages to the freeing threads). Preload
  only (with eviction).
- ✅ **`_mi_page_free_collect_partly`** — no-atomic small-block collect on the
  claim path (FR1, merged; always compiled).
- 🔵 **Delayed retire** (`retire_expire` + `_mi_theap_collect_retired` + generic
  cadence) — emptied pages retire immediately (sole page kept). Also why the
  deferred-free heartbeat is not on a deterministic alloc cadence.
- 🟡 **alloc-test (+15%)** — a heavy-cross-thread-free outlier *not* helped by
  eviction; a distinct (non-eviction) bottleneck, left for future work.
- 🟡 **Candidate search** (`mi_page_queue_find_free_ex`: prefer-fuller,
  free-emptier-mid-scan, `page_max_candidates`, move-to-front) — rs is plain
  first-fit, no move-to-front.
- 🟡 **theap/tld split**, **full stats table**, **full ~30 option table**,
  **standalone PRNG**, **per-size chunkmap binning**, **natural-align fast path**,
  **bitmap ctz/SIMD scan** — partial/follow-up as noted in §1.
- 🔵 **TLS storage** — whole `Heap` inline in TLS (faster for static
  `#[global_allocator]`; C uses a `__thread mi_heap_t*`). initial-exec for the
  preload cdylib is set via build flag (FIDELITY §4).

## 6. Verification parity

Correctness is held to the C original by: a **differential harness** vs C v3.3.2
(`tests/differential.rs`, CI-gated, all bins + edge APIs + cross-thread), **loom**
(bitmap CAS + `xthread_free` MPSC + claim-exactly-once), **Miri**
(`-Zmiri-strict-provenance`), **TSan** + **ASan** (`-Zbuild-std`), a hardened
**abort gate** (`secure`+`debug` invalid/double-free), and **fuzz** (cargo-fuzz,
ASan op-stream). Performance is gated on a pinned i7-14700K via
`scripts/perf_compare.sh` (no in-repo regression) and the fair LD_PRELOAD sweep
`scripts/mimalloc-bench.sh` (FIDELITY §4). CI runs 12 jobs across stable/nightly.
