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

### C2 — thread-local access (std `thread_local!`, not `#[thread_local]`) — `src/init.rs:69` — **untested, live**
`DEFAULT_HEAP` (alloc) and the `TID` cell behind `current_tid()` (free) are both
plain std `thread_local!`, whose `.with()` carries a lazy-init guard and is
slower than a native `__thread`/`#[thread_local]` slot. The `nightly` feature
does **not** change this (no `#[thread_local]` malloc path exists yet) — which is
why the earlier nightly run showed nothing.
- **Confirm:** flamegraph shows time in `__tls_get_addr` / the `LocalKey`
  accessor; `perf stat` low-IPC if TLS stalls.
- **Fix ideas (T2-tls):** under `nightly`, cache the heap pointer (and tid) in a
  real `#[thread_local] static` and have `init::malloc*`/`free` read it directly,
  falling back to the `thread_local!` on stable. Measure the delta.

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
