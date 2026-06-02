<!-- SPDX-License-Identifier: MIT -->
# Small-object hot-path performance notes

`bench_suite` (phase 1, single-threaded `8..1032`) shows mimalloc-rs trailing
the C reference by ~25–40% on the small alloc/free hot path, while matching or
beating it on large/huge/cross-thread and scaling slightly better across
threads. This note records the **measure-first** investigation of that gap.

> **Already ruled out by measurement** (do not re-try without new evidence):
> - **Aligned over-allocation** (`align == 16` → over-allocate `size + 15`): a
>   variant that serves ≤16-align from the natural block measured
>   *perf-neutral* (~77 Mops/s both ways). Not the bottleneck. (It is still a
>   worthwhile *memory* change, parked separately.)
> - **`--features nightly`**: measured no change — because the `nightly` feature
>   does **not** wire a `#[thread_local]` path for the default-heap malloc (see
>   C2). That run was a null test, not evidence against TLS cost.

## How to profile (mimalloc-rs only)

Use `examples/profile_alloc.rs` — it runs *only* mimalloc-rs's small alloc/free
hot loop, so a flamegraph contains only our code. On a quiet, core-pinned
machine:

```sh
RUSTFLAGS="-C debuginfo=1" cargo build --release --example profile_alloc

# Flamegraph (cargo install flamegraph):
cargo flamegraph --release --example profile_alloc          # → flamegraph.svg

# Top self-time symbols:
perf record -g --call-graph=dwarf -- ./target/release/examples/profile_alloc
perf report --stdio | head -40

# Counters — tells compute-bound from memory-bound:
perf stat -d -- ./target/release/examples/profile_alloc
#   high IPC + high instructions  ⇒ compute (e.g. the free-path divide, C1)
#   low IPC + high dTLB/branch-miss ⇒ memory/TLS (e.g. C2/C3)
```

Tunables: `PROFILE_ROUNDS`, `PROFILE_WINDOW`, `PROFILE_SIZE_MIN/SPAN`,
`PROFILE_PIN_CORE` (see the example's module docs).

## Per-cycle cost of one alloc/free (default build)

| step | alloc | free |
|---|---|---|
| TLS access | `DEFAULT_HEAP.with(...)` (std `thread_local!`) | `current_tid()` (std `thread_local!` Cell) |
| page-map | — | `page_map::lookup`: 3 `Acquire` loads (TOP → submap → entry) |
| arithmetic | `wsize_from_size`, fast-path index | **integer divide** `off / bs` (block-start normalization) |
| list op | `Page::alloc` free-list pop (`free.get` → `next` → set → `used+1`) | `Page::free_local` push + `used-1` |

So each alloc/free pair pays **2 TLS accesses**, **1 integer division**, and a
**3-load page-map walk**. The C reference uses one `__thread` theap pointer, no
divide on the free fast path, and a tuned page-map.

## Candidate hotspots (confirm/rank with the profile above)

### C1 — free-path integer division — `src/heap.rs:384` — **high confidence**
```rust
let off = ptr.addr().get() - pstart.addr();
let block_start = pstart.wrapping_add((off / bs) * bs);
```
A `/` (and `*`) on **every free** to normalize an interior pointer (from an
over-aligned alloc) back to its block start. Integer division is ~20–40 cycles;
at ~13 ns/op this is a meaningful fraction. The C reference avoids a divide here.
- **Confirm:** `perf stat` shows high IPC + the divide attributed in `perf
  annotate` of `heap::free`.
- **Fix ideas (T2-div):** (a) block-start fast path — if `off == 0` (the common
  case, true for every block-start pointer; even more common once the parked
  natural-block aligned change lands) skip the divide entirely; (b) for the rare
  interior pointer, a precomputed reciprocal-multiply per page, or store a
  `block_shift` when `bs` is a power of two. Keep mimalloc semantics (interior
  pointers from aligned allocs must still normalize).

### C2 — thread-local access (std `thread_local!`, not `#[thread_local]`) — `src/init.rs` — **TESTED → PARKED (no measured win)**
`DEFAULT_HEAP` (alloc) and the `TID` cell behind `current_tid()` (free) use std
`thread_local!`, whose `.with()` carries a lazy-init/state guard, hypothesized to
be slower than a native `__thread`/`#[thread_local]` slot.

**This was implemented and measured (the "A1a" story), then parked.** Under a
`nightly`-gated path the default-heap pointer and tid were cached in real
`#[thread_local]` slots so `malloc`/`free` read them directly (mirroring
mimalloc-C's `__thread mi_heap_t*` model), with a `Slot::drop` clearing the cache
before the heap drops (teardown-safe; code-reviewed APPROVE). A pinned-machine
microbench A/B (`MB_ALLOC=rs`, fixed 64 B, nightly cache vs `.with()`) showed
**no clear win** — the cached path was not faster, only lower-variance.

**Why (confirmed):** the malloc/free *route* already matches mimalloc-C v3 step
for step — C v3 itself uses `MI_TLS_MODEL_THREAD_LOCAL` (a `thread_local` theap
pointer + a `pthread_key` destructor), then `theap->pages_free_direct[idx]`, then
a free-list pop; rs does the identical `pages_free_direct[wsize]` → `Page::alloc`,
and `free` uses the same page-map + owner-tid routing. The only divergence A1a
closed was `.with()` vs a raw `#[thread_local]` load — and modern std
`thread_local!` is already cheap enough that closing it is neutral. So TLS access
is **not** the phase-1 bottleneck; the remaining gap is **C1 (full-page eviction /
search-queue cliff)** and **C3 (call-chain inlining)**, not TLS.

The implementation is preserved at the git tag
**`archive/a1a-thread-local-cache`** (recoverable if a future, fully-inlined hot
path or a non-Linux TLS model makes the direct slot pay off). Do not re-attempt
without new evidence that TLS access — not full-page scan / inlining — is the cost.

#### C2-update — the real TLS lever is the **TLS model**, and it only shows under `LD_PRELOAD`
The A1a null result above was measured on a **statically-linked** binary
(`profile_alloc`, and the `#[global_allocator]`-style `bench_suite`/`perf_compare`
harness). A static executable gets the **local-exec** TLS model automatically —
the fastest model, a bare `%fs`-relative load — so `.with()` vs a raw
`#[thread_local]` slot is genuinely neutral there. That hid the true cost.

Under **`LD_PRELOAD`** (the *fair* cross-allocator comparison — the C reference
and mimalloc-rs both preloaded as shared objects), the picture is different. A
Rust `cdylib` defaults to the **general-dynamic** TLS model, whose every access
is an out-of-line **`__tls_get_addr`** call. Both TLS reads on the hot path —
`DEFAULT_HEAP` (alloc) and the `TID` cell behind `current_tid()` (free) — pay it
on *every* `malloc`/`free`. mimalloc-C, even when preloaded, compiles its
`__thread` theap pointer with **initial-exec** (`MI_TLS_MODEL` /
`__attribute__((tls_model("initial-exec")))`), so it never makes that call.

Profiling larson under `LD_PRELOAD` on the pinned i7-14700K confirmed it:
`__tls_get_addr` (+ its PLT stub) accounted for **~5–6%** of cycles, and building
the preload `cdylib` with `-Z tls-model=initial-exec` removed it — a direct
`%fs`-relative load, matching C. Measured medians (LD_PRELOAD, vs C `libmimalloc`):

| workload | default (general-dynamic) | initial-exec | gap to C: before → after |
|----------|---------------------------|--------------|--------------------------|
| larson (8T) | 200 Mops/s | ~214 Mops/s | −10.4% → **−4.3%** |
| cfrac (1T)  | 1.74 s     | 1.62 s      | −13.7% → **−5.9%** |
| espresso (1T) | 2.91 s   | 2.84 s      | −5.4% → **−2.9%** |
| mstress (8T) | ≈C        | =C          | — |

initial-exec is safe for a preloaded library (it is loaded at program startup,
when the dynamic linker still sizes the static TLS block). The fix therefore
lives in the **preload build recipe** (`scripts/mimalloc-bench.sh`,
`scripts/preload-check.sh`), applied only on a nightly toolchain (the `-Z` flag),
and changes **no source** — so the statically-linked path (already local-exec)
and the in-repo `perf_compare` gate are untouched. See `docs/benchmarking.md`.

**TLS footprint (deferred, not a perf change).** `DEFAULT_HEAP` stores the whole
`Heap` *inline* in TLS (a few KB), whereas C keeps a `__thread mi_heap_t*`
*pointer* (8 B) and allocates the heap out of line. The inline form is actually
*faster* for the dominant static `#[global_allocator]` use — TLS access is a
direct address, with no pointer dereference. Moving the heap out of line would
make initial-exec usable even when the library is `dlopen`-ed at runtime (not
just `LD_PRELOAD`-ed), and would match C more closely, but it adds one
indirection per access and risks regressing the static phase-1 hot path for no
benchmark gain. Left as documented future work; `dlopen` users who hit a static
TLS-block error should build with the default (general-dynamic) model.

### C3 — call-chain inlining — `init::malloc_aligned → DEFAULT_HEAP.with(closure) → Heap::alloc_aligned → alloc → alloc_impl → Page::alloc` — **needs asm**
Six layers plus the `.with` closure. If the chain does not collapse, every alloc
pays call overhead the C fast path does not.
- **Confirm:** `cargo install cargo-show-asm`, then
  `cargo asm --release --example profile_alloc` (or `cargo asm mimalloc_rs::init::malloc_aligned`)
  and check the hot path is one inlined body, not a call ladder. Also inspect
  whether `pages_free_direct[wsize]` keeps a bounds check.
- **Fix ideas (T2-inline / T2-bounds):** `#[inline]` the chain end-to-end;
  `get_unchecked` on the fast-path array index with a justifying SAFETY note
  (wsize is bounded by construction).

### C4 — `Block::next(keys)` / encoding — **RULED OUT for the default build**
In the non-`secure`/`debug` build, `Block::next`/`set_next` are a plain
`Cell<*mut Block>` get/set with `keys` unused (`src/free_list.rs:36-66`). No
cost on the benchmarked path. (Only `secure`/`debug` pay the rotate/xor.)

## Method / constraints for T2

- One optimization per PR; **measure before/after on a pinned, quiet machine**
  (min + median, in-run rs/c ratio to cancel box noise). Keep a change only if
  it helps.
- Preserve mimalloc semantics exactly; lock behavior with the existing tests +
  Miri (strict-provenance) + loom + the cross-thread TSan stress.
- This repo's CI box cannot profile (`perf_event_paranoid` high) and is noisy —
  authoritative measurements come from a controlled local run.

## Isolation experiment (`examples/microbench.rs`) — instruction-count A/B

`profile_alloc`/`bench_suite` mix per-iteration overhead (RNG, `Vec::swap_remove`,
`Layout`, touch) into the loop. `microbench` strips all of it: a fixed size, a
precomputed `Layout`, and a power-of-two ring (`i & mask`) — one alloc + one
free per iter with only a mask/index/branch/store around them. It runs **one
allocator per process** (`MB_ALLOC=rs|c|system`) so `perf stat` attributes its
counters cleanly. Compare instruction counts:

```sh
RUSTFLAGS="-C debuginfo=1 -C force-frame-pointers=yes" cargo build --release --example microbench
MB_ALLOC=rs perf stat -e instructions,cycles,L1-dcache-load-misses -- ./target/release/examples/microbench
MIMALLOC_C_LIB=<dir> MB_ALLOC=c perf stat -e instructions,cycles,L1-dcache-load-misses -- ./target/release/examples/microbench
# per-op (alloc+free) instruction delta = (rs_insn - c_insn) / (2 * MB_ROUNDS)
```
Reads: rs **more instructions** ⇒ leaner fast path can win (code); rs **similar
instructions but more cycles/L1-misses** ⇒ microarchitectural (cache/TLS/ports),
little to gain from shaving instructions. (The C reference is FFI-indirect, so a
few thunk instructions are charged to `c` — biased *against* "rs has more".)

### Finding so far (fixed vs mixed size)
On the dev box, the **fixed-size** microbench (`size=64`) shows rs ≈ **91%** of
C (444 vs 488 Mops/s — only ~9% behind), whereas `bench_suite` phase 1 (**mixed**
`8..1032`) shows ~74%. So the gap is small on a single hot size class and
**widens with size diversity** — pointing at the memory side (more bins → more
pages → the 13.9% L1-miss working set) rather than a single hot instruction.
This is consistent with the three point-fixes that measured **neutral**
(aligned over-alloc, nightly TLS null-test, free-path divide skip): no single op
dominates. The instruction-count A/B above decides whether the remaining lever
is code-leanness or microarchitecture.

## Checkpoint: confirmed root cause + plan (2026-06)

The instruction-count A/B (`microbench`, `perf stat`, dev box, core-pinned) was
decisive. Per-size, mimalloc-rs vs the C reference (`MB_ROUNDS=5e7`):

| size | rs Mops (IPC) | c Mops (IPC) | live pages | rs insn | c insn |
|---:|---|---|---:|---:|---:|
| 256 | 265 (2.99) | 500 (4.11) | 8 | 6.0 B | 4.5 B |
| 512 | 256 (2.83) | 440 (4.04) | 16 | 6.1 B | 5.1 B |
| 768 | 269 (3.02) | 236 (4.48) | 21 | 6.1 B | 10.4 B (C worse here) |
| 1024 | **11.9 (0.48)** | 198 (4.67) | 32 | **22.4 B** | 12.9 B |

The C reference holds **IPC ≈ 4.0–4.7 at every size — no cliff**. mimalloc-rs's
IPC falls as the live-page count grows and **collapses at size 1024** (IPC 0.48,
instructions 3.7× higher, L1-misses ~1000× higher). Both *more instructions*
(scan work) and *more stalls* (cache).

Two **missing mimalloc mechanisms** explain the gap (both real, both absent):

1. **Full-page eviction (the search-queue `MI_BIN_FULL`).** `Heap::alloc_impl`
   keeps full pages in the per-bin queue and merely *skips* them while scanning,
   so every alloc re-walks all full pages — ~O(live pages). This is the
   instruction explosion (22 B at size 1024). mimalloc moves full pages to a
   dedicated full queue so the search stays short, and uses a **delayed-free
   protocol** (atomic `xthread_free` "delayed" state + a per-heap
   thread-delayed-free list) to bring a full page back when a *cross-thread*
   free targets it.
   - A first attempt (move full pages to `pages[MI_BIN_FULL]`; un-full on local
     free; rely on `collect` for cross-thread frees instead of delayed-free)
     **regressed cross-thread correctness**: the secure+debug parallel suite's
     intermittent "invalid free / not owned" abort rose from **1/12 (main) to
     6/12**. Root cause class: without delayed-free, a full page off the search
     queue can be retired/relocated in a window that races a cross-thread free
     (the retire-vs-free happens-before that `used==0` normally guarantees). The
     attempt was **not merged** (parked/discarded). Conclusion: full-page
     eviction **requires** the delayed-free protocol to be cross-thread-safe.

2. **Lazy page extend (`capacity` vs `reserved`).** `Page::init` →
   `build_free_list` threads **all** `reserved` blocks upfront, touching every
   block's first word across the whole page (e.g. 64 cache lines for a 64 KiB
   small page) and scattering the free list across the page → cold-cache writes
   at page creation and cache-unfriendly traversal. mimalloc inits `capacity=0`
   and **extends the free list in batches** (`mi_page_extend_free`,
   ~`MI_MAX_EXTEND_SIZE/bsize` blocks at a time) from the generic alloc path, so
   only the memory actually used is initialized and stays hot. The `Page`
   already carries a (currently vestigial) `capacity` field set to `reserved`.
   This is **owner-only page state — no cross-thread hazard** — so it is the
   lower-risk lever, and it directly targets the IPC/L1-miss collapse.

### Plan (perf round, ordered low-risk → high-risk)
1. **Lazy page extend** — make `Page::init` start `capacity=0` and add
   `mi_page_extend_free`-style batched extension on the generic alloc path
   (owner-only). Re-measure size 1024 / mixed. *(safe; do first)*
2. **Full-page eviction + delayed-free** — port mimalloc's full queue together
   with the delayed-free protocol (atomic `xthread_free` delayed state + per-heap
   thread-delayed-free list) so cross-thread frees into off-queue full pages are
   handled correctly. Gate on the cross-thread tests + loom + TSan + the
   secure+debug parallel abort rate (must return to ≤ main's 1/12, ideally 0).
   *(complex; concurrency-critical)*

Method unchanged: one change per PR, measured before/after on a pinned machine,
behavior locked by tests + Miri + loom + the cross-thread TSan stress.

## Practicality round: delayed purge + on-demand commit (2026-06)

mimalloc-rs now **returns freed memory to the OS** like v3 (previously freed
arena slices were reused but stayed resident — RSS was sticky at the
high-water mark). See `docs/GOAL-purge-commit.md` for the staged plan (PC0–PC3).

- **PC0** `os::purge_ex` (reset vs decommit → `needs_recommit`) + v3 option
  defaults (`purge_delay=1000`, `purge_decommits=1`, `arena_purge_mult=1`).
- **PC1** the arena commit bitmap is authoritative (eager arenas pre-set all
  bits → no per-slice commit on the fast path), so recommit-after-purge is
  correct even under `debug`/`secure` (where decommit strips access).
- **PC2** per-arena purge bitmap + `purge_expire`: `free_slices` schedules a
  delayed purge; `collect`/`retire_page` drive it; `run_purge` claims each
  still-free range from the free bitmap before `madvise` (no alloc can race it).
- **PC3** `examples/rss_spike.rs` proves it.

**RSS evidence** (`rss_spike`, 128 MiB working set, this box):

| phase | purge on | purge off (`MIMALLOC_PURGE_DELAY=-1`) |
|---|---|---|
| peak (touched) | 129.5 MiB | 129.5 MiB |
| after free | 129.5 MiB | 129.5 MiB |
| after `collect(true)` | **1.7 MiB** | 129.5 MiB |
| reclaimed | **100% of the spike** | 0% |

**Linux residency caveat.** On Linux/overcommit `commit` is just `mprotect`;
pages become resident on first touch, so *lazy commit* barely moves RSS — the
real lever is **purge** (`MADV_DONTNEED`). Purging is delayed (default 1 s) and
driven by alloc/free/collect, never a background thread (matching v3); a tight
loop that frees then idles without collecting keeps its pages until the next
`collect`/allocation, exactly as in v3.
