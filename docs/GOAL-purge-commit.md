<!-- SPDX-License-Identifier: MIT -->
# GOAL — Practicality round: delayed purge + on-demand commit (faithful v3 port)

> Round brief for `omc ultragoal`. Make mimalloc-rs return freed memory to the OS
> like mimalloc v3 does, so it is a production-grade global allocator (bounded
> **RSS**, not just bounded address space). Port v3's mechanism faithfully, in
> idiomatic + simple Rust.

## Why (problem statement)

Today `Arena::free_slices` only flips the free bitmap. Freed slices are reused,
but **never returned to the OS** — no `decommit`/`reset`/`MADV_DONTNEED` on the
retire path. So RSS is sticky at the high-water mark: a transient allocation
spike keeps its pages resident forever. Correct and leak-free, but not
production-grade for spiky/long-running processes (see `docs/perf-hotpath.md` for
the retire path; `os::purge/decommit/reset` already exist but are unwired).

**Key platform insight (drives priority).** On Linux with overcommit, `commit`
is just `mprotect(R|W)`; pages become *resident* only on first touch. So "eager
commit" of a 256 MiB arena costs ~0 RSS until touched — **lazy commit barely
moves RSS on Linux**. The lever that actually drops RSS is **purge**
(`MADV_DONTNEED`), which evicts resident pages (they fault back as zero on reuse).
Therefore: **delayed purge is the primary deliverable (the RSS win); on-demand
commit is secondary** (correct commit accounting, Windows/no-overcommit, v3
parity).

## Non-negotiable principles

1. **Faithful v3 port.** Mirror `src/arena.c` purge/commit semantics exactly
   (citations below). Same option names/defaults, same reset-vs-decommit
   decision, same claim-from-free purge protocol, same delay model.
2. **Idiomatic Rust, Rust strengths.** `needs_recommit` as a return value (not a
   C out-param); option-driven behavior through the existing `options` module;
   clock through the existing `prim::clock_now_msecs` trait method; encapsulate
   the unsafe OS calls behind typed `os::*`; `// SAFETY:` on every new unsafe.
3. **Simple, clean code.** Reuse the existing `Bitmap` and meta-allocation
   layout (a third symmetric bitmap), the existing `collect` heartbeat as the
   drive point. Prefer the smallest correct mechanism; skip v3's micro-throttles
   (e.g. the subproc-level `purge_expire` cache) unless a measurement demands them.

## ABSOLUTE bar (carried from the perf round)

- **No performance regression** vs pre-round `main` on **any** `bench_suite`
  phase — purge/commit must stay off the alloc/free fast path. Authoritative
  measurement on the user's pinned machine via `scripts/perf_compare.sh` (this
  env is too noisy). Merge a stage only if no regression.
- **RSS must measurably drop** on a spike-then-idle workload (new bench).
- Correctness locked: cross-thread stress + TSan + loom (the purge claim/free
  race) + the secure+debug parallel abort gate must stay at main's level (0).
- The V-round (lock-free abandoned + cliff) was discarded for a small MT
  regression; its code is archived at local tag `archive/v-round-v1v2`. Do not
  resurrect it here.

## v3 reference (cite when porting)

- Lazy commit: `mi_option_arena_eager_commit` (`src/arena.c:355`, values 0/1/2 —
  `2` = commit only on overcommit/large-pages); on-demand commit in
  `mi_arena_try_alloc_at` (`src/arena.c:236-295`) via the `slices_committed`
  bitmap; page-level partial commit (`src/arena.c:886-895`).
- Delayed purge: options `purge_delay` (1000 ms), `arena_purge_mult` (1),
  `purge_decommits` (1) — `src/options.c:127,140,149`. Schedule on free:
  `mi_arena_schedule_purge` (`src/arena.c:1279, 2059-2083`) sets `purge_expire`
  (CAS from 0) + the `slices_purge` bitmap. Execute: `mi_arenas_try_purge` /
  `mi_arena_try_purge` (`src/arena.c:2133-2208`) — expiry check, then per range
  **claim from `slices_free`** (`mi_bbitmap_try_clearNC`), `mi_arena_purge`,
  release back to free (`src/arena.c:2092-2127`). OS: `mi_arena_purge`
  (`src/arena.c:2028-2054`) + `_mi_os_purge_ex` (`src/os.c:640-663`) — reset vs
  decommit by `purge_decommits` + `allow_reset` (= all committed), returns
  `needs_recommit`. Driven by `_mi_arenas_collect` at page abandon/retire/heap
  collect (`src/page.c:312,419`, `src/theap.c:135`) — **no background thread**;
  clock is `_mi_clock_now()` msecs.

## Current rs state (what exists / what's missing)

Exists: `os::{commit→is_zero, decommit, reset, purge=decommit}`,
`prim::clock_now_msecs` (CLOCK_MONOTONIC), `Arena` free + commit bitmaps with
on-demand `ensure_committed` (gated by `eager_committed`), options
`EagerCommit`(=1), `PurgeDecommits`(=0), `PurgeDelay`(=10), `Subproc` arena
registry + `collect` heartbeat (`init::collect`→`run_deferred_free`).

Missing / wrong: no `slices_purge` bitmap, no `purge_expire`, no
`schedule_purge`/`try_purge`; `free_slices` doesn't schedule; `os::purge` is a
blind decommit with no reset path and no `needs_recommit`; option defaults differ
from v3 (`PurgeDelay` 10 vs 1000, `PurgeDecommits` 0 vs 1) and aren't wired; no
`arena_purge_mult`; `EagerCommit` default 1 (always eager) vs v3 2.

## Milestones (low-risk → high-risk; one PR each, measured)

### PC0 — OS purge primitive + option wiring  *(low risk; pure addition)*
- `os::purge_ex(addr, size, allow_reset: bool) -> bool` (`needs_recommit`):
  if `PurgeDelay < 0` → no-op return false; else if `PurgeDecommits` enabled →
  `decommit` (returns `true`); else if `allow_reset` → `reset` (returns `false`);
  preserve the page-conservative rounding already in `os`. Keep `os::purge` as
  `purge_ex(.., true)` for callers that don't track commit.
- Wire option **defaults to v3**: `PurgeDelay`=1000, `PurgeDecommits`=1; add
  `ArenaPurgeMult`=1 and `arena_purge_delay() = purge_delay * arena_purge_mult`.
  Decide `EagerCommit` default in PC1.
- **DoD:** unit tests for reset vs decommit + `needs_recommit`; `purge_delay=-1`
  disables; clippy/fmt; no behavior change yet (nothing calls `purge_ex`).

### PC1 — on-demand commit alignment (lazy commit)  *(low–medium; infra exists)*
- Make commit-on-demand the default per v3 `arena_eager_commit` semantics
  (`EagerCommit` default → v3's 2 = eager only when overcommit/large-pages; on a
  no-overcommit prim, reserve-then-commit-on-alloc). `ensure_committed` already
  does the per-range commit; make `initially_zero`/`is_zero` accounting correct
  so a freshly committed (or recommitted-after-decommit) range zeroes only when
  the OS didn't. Recommit-after-purge already works (commit bit cleared →
  `ensure_committed` re-commits).
- **DoD:** reserve→commit-on-alloc→write→free→realloc roundtrip; commit bitmap
  popcount correctness test; `bench_suite` **no regression** (commit only on the
  alloc slow path, never the fast path). Note in the PR that the Linux RSS effect
  is small by design (residency is touch-driven) — value is accounting + Windows.

### PC2 — delayed purge scheduler  *(HIGH risk — the RSS win)*
- `Arena`: add a third symmetric **purge bitmap** (extend the meta layout from
  `2*(chunk_count+1)` to `3*(chunk_count+1)` BChunks) and `purge_expire: AtomicI64`.
- `Arena::free_slices` → also `schedule_purge(idx, n)`: set purge bits; CAS
  `purge_expire` 0→`clock_now_msecs() + arena_purge_delay()`. If `purge_delay==0`
  purge immediately; if `<0` skip (pinned arenas always skip).
- `Arena::try_purge(now, force) -> bool`: if `!force && (expire==0 || expire>now)`
  return; clear `expire`; for each set range in the purge bitmap: **claim it from
  the free bitmap** (`try_clear_n` at that index — add this targeted bitmap op if
  absent) so no allocator can take it mid-purge; on claim, `os::purge_ex(p, size,
  all_committed)`, then if `needs_recommit` clear the commit bits (else leave set
  — reset stays committed), then **release the range back to the free bitmap**;
  clear the purge bit. If claim fails (reallocated), just clear the purge bit.
- `Subproc::try_purge(force)`: iterate registered arenas, each gated by a cheap
  `purge_expire` atomic load (skip v3's subproc-level expire cache for simplicity
  unless measured necessary). Drive it from the existing **`collect`** and from
  the page-lifecycle points that mirror v3 (`retire_page` / `release_page_slices`
  / `Drop`, and the alloc slow path `new_page`) — throttled so the common path
  pays one atomic load. Apply a minimum-purge-size guard (avoid THP fragmenting).
- **DoD (gated):** loom model of schedule↔claim↔alloc race (no double-purge, no
  purge-of-live-slice, no lost free); cross-thread TSan stress; secure+debug
  parallel abort gate = 0; miri on the bitmap ops; option matrix
  (`purge_delay` = -1 / 0 / default) behaves; **`bench_suite` no regression** on
  any phase (pinned machine).

### PC3 — verify + lock  *(measurement + docs)*
- New **RSS bench** (e.g. `examples/rss_spike.rs`): allocate a large set, free
  it, idle/collect, sample RSS (`/proc/self/statm`); assert RSS drops toward the
  steady-state set with purge on, and stays high with `purge_delay=-1`. Compare
  against C mimalloc qualitatively.
- Pinned-machine `scripts/perf_compare.sh` (extend to also report peak RSS):
  confirm no time regression AND the RSS win. C differential parity on observable
  invariants (alignment/zero/usable_size/no-overlap/no-leak) unaffected.
- Update `docs/perf-hotpath.md` / module docs; record defaults and the
  Linux-residency caveat.

## Concurrency hazards to get right (PC2)

- **Claim-before-purge.** A freed slice is in *both* the free bitmap
  (immediately reusable) and the purge bitmap. `try_purge` MUST `try_clear_n` it
  from the free bitmap first; only purge if the claim succeeds, then set it free
  again. Skipping the claim races a concurrent `alloc_slices` (purge of live
  memory). Mirrors `mi_arena_try_purge_range` (`src/arena.c:2092-2106`).
- **Commit-bit ordering.** After decommit, clear commit bits *before* releasing
  the slice back to free, so a subsequent `ensure_committed` re-commits. After
  reset, leave commit bits set (still mapped); reuse just touches zeroed pages.
- **Clock unavailability.** `clock_now_msecs` is std/prim-only; the purge path is
  the std collect path, so this is fine. Do not call it on the no_std fast path.
- **`Date.now`-style determinism for tests.** Drive loom/unit purge tests with an
  injected `now`, not the real clock (keep `try_purge(now, force)` taking `now`).

## Definition of done (round)

All PCs merged (or explicitly parked); RSS demonstrably returns to OS on the new
bench; **zero regression on any `bench_suite` phase (pinned machine)**;
loom+miri+TSan+abort gates green; options match v3 defaults; docs updated.
