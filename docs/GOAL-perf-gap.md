<!-- SPDX-License-Identifier: MIT -->
# GOAL — PG: close the alloc-test free-path parity gap vs mimalloc-C

**Round owner:** assistant (measures on the pinned i7-14700K)
**SSOT for this round.** Memory: `mimalloc-rs-vround-perf.md`. Baseline `main = 1beb07e`
(FR1+FR2+FR3+FR4 merged; full-page eviction + cross-thread reclaim live for the
preload cdylib).

## 1. Objective

Close the remaining benchmark-parity gap to mimalloc v3.3.2 under a *fair*
LD_PRELOAD comparison. After FE2 the suite is within ~2–4% on every workload
**except `alloc-test`, which is +15% slower** (the lone outlier). This round
targets that gap.

Three governing principles (every story):
1. **Faithful v3 port** — cite the C source (`free.c`, `page-map.c`,
   `internal.h`); match the mechanism, not just the result.
2. **Idiomatic Rust** — leverage the type system / RAII / encapsulated `unsafe`;
   where a faithful-but-unsafe C shortcut conflicts with Rust safety, redesign
   it the Rust way (keep the speed *and* the safety).
3. **Simple & clean** — smallest diff that closes the gap; no speculative
   machinery.

## 2. Evidence (pinned i7-14700K, 2026-06-03)

- `alloc-test 8`, interleaved ×4: **rs 2.98–3.03 s vs C 2.60–2.62 s = +15%**,
  noise ≈ 0 (stable, reproducible).
- Userspace profile (`perf`, fp call-graph):
  - rs `heap::free` **12.76 %** (top symbol) vs C `operator delete[]` (inlined
    `mi_free`) **11.18 %**. In **absolute** time: free ≈ 0.383 s vs 0.291 s →
    **the free path is ~32 % heavier**. Alloc is ≈ par (`mi_new_nothrow` 9.55 %
    vs `operator new[]` 10.64 % — rs alloc is marginally *faster*).
  - rs `heap::free` annotate hot spots:
    - `mov 0x30(%rbx),%rsi` **27 %** — load `page.block_size` = the **cold
      page-header cache line** (xthread_id@0 + block_size@0x30 share it). A miss
      inherent to touching page metadata on a cross/cold free.
    - `mov (%rax,%rcx,8),%rbx` **10 %** — the 3rd dependent page-map load
      (the page pointer).
    - `test/je` after the TOP / submap loads — the **defensive null branches**
      (~1–2.5 % each).
  - C `mi_free` region shows the **same** structure: a page-header miss
    (`mov 0x28(%rsi)`, 57 % local) + page-map loads. **C does not avoid the
    cache misses.**
- page-faults rs 3078 vs C 2841 (+8 %), ctx-switches 115 vs 57 — small absolute,
  not the dominant cause.

### Root cause (corrected)

The earlier "C uses a flat page-map, rs uses 2-level" lead was a **misread** of
`bits.h`: `MI_PAGE_MAP_FLAT` is enabled only for `MI_MAX_VABITS <= 40`; **x64 is
47 → C also uses the 2-level page-map**. The page-map *structure* and the
cache-miss pattern are identical between rs and C.

The differential is in **codegen on the free fast path**:
- C release/non-secure uses `_mi_unchecked_ptr_page` — **zero null branches**
  (`(_mi_page_map_at(idx))[sub_idx]`, relaxed loads). It tolerates a foreign
  pointer crash because, in this benchmark, every freed pointer is its own.
- rs `page_map::lookup` carries **3 defensive branches** (TOP null / top-idx
  range / submap null) so an LD_PRELOAD foreign `free(p)` is safe, plus a 4th
  `page_ptr.is_null()` route in `free`.

So rs pays extra branches (and the codegen they force) that C's unchecked path
skips. The page-header miss (27 %) is inherent and present in both — **not** the
lever.

## 3. Plan (stories, each its own PR; main is PR+CI protected)

### PG1 — branch-free safe lookup via a shared zero-submap *(primary lever)*
Make the hot `lookup` branch-free **without** sacrificing foreign-pointer
safety, mirroring C's `_mi_unchecked_ptr_page` shape:
- **Eager-init the top table** at process init (C does: `_mi_page_map_init`),
  so `TOP` is never null on the hot path → drop the `TOP.is_null()` branch.
- **Shared zero-submap**: point every *unregistered* top entry at one shared,
  read-only, all-null submap (generalizes C's committed entry-0 `sub0` /
  `_mi_page_map[0]=1` NULL-resolution trick). Then `submap[sub_idx]` is always a
  valid read (returns a null page for foreign/unmapped addresses) → drop the
  `submap.is_null()` branch with **no crash risk**. `register`/`unregister`
  swap a top entry between the shared zero-submap and a real submap.
- Drop the `top_idx >= TOP_COUNT` branch on the unchecked path (canonical x64
  addresses are always in range; keep the bounds check only in the `secure` /
  non-override `_mi_safe_ptr_page` equivalent).
- Net: hot `lookup` = 3 loads, **0 branches** (the single `page.is_null()` in
  `free` remains — C has it too). Faithful (C intent), idiomatic (safe by
  construction, not by luck), simple.

Risk: `page_map.rs` is on **both** the static `#[global_allocator]` and preload
hot paths. Expect a strict improvement for both; if `perf_compare` shows any
static regression, `cfg(any(override_export, test))`-gate the unchecked variant
(A2/FE2 pattern) so the static build stays byte-identical.

### PG2 — free-dispatch codegen tightening *(only if PG1 leaves a gap)*
Audit `heap::free` codegen vs C `mi_free_ex`: redundant reloads of page fields,
the `recover_block` closure, stats hook placement on the hot path. Trim to match
C's "written carefully to prevent register spilling" fast path.

### PG3 *(documented alternative, default-off / parked unless PG1+PG2 insufficient)*
Flat page-map under `override_export` only: 2 loads + arithmetic page base (no
3rd load), at the cost of a ~2 GB virtual reserve with on-demand commit. This is
a **divergence** from C-on-x64 (which chose 2-level for exactly this reserve
cost), so it ships only behind a documented option, never on the static path.

### PG4 — verify + lock
Re-run the full fair sweep + `perf_compare`; update `FIDELITY.md §4`,
`PORTING-STATUS`, and the memory; final ai-slop-cleaner + verifier/code-review.

## 4. Absolute gates (every merge)

- **Fair LD_PRELOAD sweep** (both preloaded, interleaved median, pinned, cores
  2–9): `alloc-test` **must improve**; cfrac / espresso / mstress / rptest /
  larson / malloc-large / xmalloc-test **no regression**.
- **`MIMALLOC_C_LIB=/tmp/mimalloc-c/build bash scripts/perf_compare.sh 13 2`**
  (static rs-vs-main): **no phase regression** (phase 2/4 noise ±8% — read min
  **and** median, multiple runs).
- Merge a story only if sweep nets positive **AND** perf_compare no-regression.
  If they conflict, **STOP** and present the tradeoff with numbers — never force
  or silently regress.
- Correctness, all green: `cargo test` (+ secure/debug/stats/track), abort gate,
  differential vs C v3.3.2 (`MIMALLOC_C_LIB`), loom, TSan (`-Zbuild-std`), Miri
  (strict-provenance). New lookup invariants tested (foreign/unmapped → null,
  no crash; register/unregister round-trip through the shared zero-submap).
- `cargo fmt --all` before every push.

## 5. Reproduction

```sh
# cdylib (preload)
RUSTFLAGS="--cfg override_export -Z tls-model=initial-exec -D warnings" \
  cargo rustc --release --features override --crate-type cdylib --target-dir target/preload
# => target/preload/release/libmimalloc_rs.so   (C lib: /tmp/mimalloc-c/build/libmimalloc.so)

# alloc-test (seconds; lower better), interleaved rs vs C
taskset -c 2-9 env LD_PRELOAD=<so> bash -c "/usr/bin/time -f '%e' ./alloc-test 8" 2>&1 >/dev/null | grep -E '^[0-9.]+$' | tail -1

# profile
perf record --call-graph fp -- taskset -c 2-9 env LD_PRELOAD=<so> ./alloc-test 8
perf annotate --stdio -d libmimalloc_rs.so 'mimalloc_rs::heap::free'

# no-regression gate
MIMALLOC_C_LIB=/tmp/mimalloc-c/build bash scripts/perf_compare.sh 13 2
```
