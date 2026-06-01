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
