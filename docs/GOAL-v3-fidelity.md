<!-- SPDX-License-Identifier: MIT -->
# GOAL — v3 fidelity round: close the audited behavioral divergences

> Round brief for `omc ultragoal`. A two-agent audit of the port against mimalloc
> v3 (`docs/` PR notes; both repos read side-by-side) found several **behavioral
> divergences** in the core alloc/retire/page-search paths that explain the
> remaining perf gap vs C (and why the parked full-page-eviction attempt, FE2,
> backfired). This round ports those v3 mechanisms **faithfully** — idiomatic,
> simple, clean Rust — closing the gaps. No correctness bugs were found; these are
> all perf-relevant fidelity gaps.

## Why (audit findings, prioritized)

Baseline = `main` (FE0 flag-folded free + FE1a bitmap registry + FE1b
ownership/collect-on-free, all merged). FE2 (eviction) is **parked**
(`archive/fe2-full-page-eviction`) — net-negative.

- **VF1 — delayed retire (`retire_expire`).** *The biggest divergence.* v3
  `_mi_page_retire` does **not** free an emptied page; it sets `retire_expire =
  MI_RETIRE_CYCLES (16)` (small) / `/4` (larger) when the page is the sole page of
  its size class, and `_mi_theap_collect_retired` frees it later (decremented per
  collect pass), resetting `retire_expire = 0` the instant the page is re-served.
  The port frees emptied pages **immediately** (keeping only the sole page),
  forcing an arena round-trip + page-map re-register on every alloc/free/alloc
  cycle. (`page.c:432-496` vs `heap.rs:retire_page`.) Hits phase 1/2/4 churn.
- **VF2 — candidate page search.** v3 `mi_page_queue_find_free_ex` is not
  first-fit: it scans up to `page_max_candidates (4)`, prefers the **fuller**
  non-mostly-used page (drains emptier ones), **frees all-free candidates** found
  mid-scan, and **moves the chosen page to the queue front** (`retire_expire=0`)
  so the `pq->first` quick path hits next time. The port is plain first-fit, no
  move-to-front. (`page.c:744-854` vs `heap.rs:find-free scan`.) Affects RSS,
  reuse, and the medium/large path (which has no `pages_free_direct` fast path).
- **VF3 — cross-thread collect cost.** (a) v3 uses `_mi_page_free_collect_partly`
  (no atomic) for small blocks on the claim path; the port always full-collects
  (a CAS swap-drain per claimed cross-thread free). (b) v3's reclaim-on-free has a
  `max_reclaim`/queue-length cap + threadpool gate; (c) v3 short-circuits the
  abandoned search with an atomic `abandoned_count[bin] == 0` check. (`free.c`,
  `arena.c` vs `heap.rs`/`subproc.rs`.) Mostly phase 4 / MT.

## Non-negotiable principles

1. **Faithful v3 port.** Mirror the cited C functions' *observable behavior*
   (retire timing, candidate selection, collect cadence). Same option
   names/defaults (`page_full_retain=2`, `page_max_candidates=4`,
   `MI_RETIRE_CYCLES=16`, `generic_collect=10000`, `page_reclaim_on_free=0`).
2. **Idiomatic Rust, Rust strengths.** Encapsulate the new page-lifecycle state
   (`retire_expire`, `page_retired_min/max`) in typed fields/`Cell`s; the
   candidate scan as a clean loop returning a chosen page; `// SAFETY:` on each
   new unsafe; no panics on the hot path.
3. **Simple, clean code.** Prefer the smallest faithful mechanism. Don't port
   knobs that don't move the bench (e.g. the `is_in_threadpool` reclaim split)
   unless a measurement demands them — note any such omission.

## ABSOLUTE bar

- **No regression on any `bench_suite` phase** vs `main`. **Authoritative
  measurement is run in-repo on this pinned machine** (`scripts/perf_compare.sh
  9 2`) by the assistant directly, each story. A story merges only if no phase
  regresses; ideally phase 1/2 improve.
- Correctness locked at main's level: `cargo test` (incl. cross-thread),
  hardened **abort gate**, **differential** vs C v3.3.2, **loom**, **TSan** all
  stay green (0). New page-lifecycle invariants get focused tests.
- Anything that cannot clear the no-regression bar is parked (tag + documented),
  not forced through.

## Milestones (one PR each, assistant-measured)

### VF1 — delayed retire + collect cadence  *(highest expected payoff)*
- `Page`: add `retire_expire` (Cell<u8/u16>). `_mi_page_retire` port: on a page
  emptying, if it is the sole page of a non-special bin, set `retire_expire`
  (16/4) and **keep** it (don't free); otherwise free immediately. Reset
  `retire_expire = 0` whenever the page is re-served.
- `Heap`: `page_retired_min/max` range; `collect_retired(force)` port
  (`_mi_theap_collect_retired`) decrementing expiries and freeing at 0; the
  generic-alloc **cadence** (`generic_count`/`generic_collect`) driving
  `collect_retired` + `run_deferred_free` every ~1000 generic allocs and a full
  `collect` every ~10000 — so retired pages are actually swept.
- DoD: `bench_suite` **no regression, phase 1/2 ideally improve** (pinned, by me);
  a focused test that an alloc-all/free-all/alloc-all cycle reuses pages (no arena
  churn); all correctness gates green.

### VF2 — candidate page search  *(after VF1)*
- Port `mi_page_queue_find_free_ex`: candidate loop with `page_max_candidates`,
  prefer fuller non-mostly-used pages, free all-free candidates mid-scan,
  move chosen page to front (`mi_page_queue_move_to_front`). Add
  `Page::is_mostly_used` (7/8). Keep the `pq->first` quick-collect fast path.
- DoD: no regression (pinned); RSS not worse (`examples/rss_spike.rs`);
  correctness gates green.

### VF3 — cross-thread collect cost  *(after VF2; measure necessity first)*
- `_mi_page_free_collect_partly` (no-atomic small claim), a `max_reclaim` cap on
  reclaim-on-free, and an `abandoned_count[bin]` atomic fast-path before the
  arena scan. Only land the parts that measurably help phase 4 (skip the rest,
  documented).
- DoD: phase 4 improves or is neutral; no regression elsewhere.

### VF4 — verify + lock
- Full pinned `perf_compare` across all phases; update `docs/perf-hotpath.md`
  (mark the divergences closed), module docs, `CHANGELOG`, memory. Final gate:
  ai-slop-cleaner + verifier/code-review (writer/reviewer separated).

## Definition of done (round)
All VFs merged (or explicitly parked); no `bench_suite` phase regresses on the
pinned machine and phase 1/2 improve where the mechanism applies; the audited
divergences are closed or documented; loom + miri + TSan + differential + abort
gates green; docs + memory updated.
