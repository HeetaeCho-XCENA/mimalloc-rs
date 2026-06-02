<!-- SPDX-License-Identifier: MIT -->
# GOAL — Full-page eviction + delayed-free (faithful v3 port)

> Round brief for `omc ultragoal`. Fix the phase-1 (single-thread) small-alloc
> **−28%** gap vs mimalloc C v3 by porting v3's full-page eviction — which is only
> safe with v3's ownership-tagged `xthread_free` "delayed-free" protocol. Faithful
> v3 port, idiomatic + simple Rust, **zero regression on any `bench_suite` phase**.

## Why (problem statement)

The bin queue is **never pruned of full pages**. When the `pages_free_direct`
fast path misses (the current page filled), `Heap::alloc_impl` linearly scans
`self.pages[b]` calling `Page::is_full()` on each page — and `is_full()`, when the
`free` list is empty, runs a full `collect()` (an atomic `xthread_free` swap +
`local_free` splice). As pages fill and stay in the queue, the scan becomes
`O(total pages) × atomic`, which is the measured phase-1 small regression
(`heap.rs:218-236`, `page.rs:454-464`; see `docs/perf-hotpath.md` §C1).

C v3 keeps the queue short: a page that fills is **evicted** from its size-class
queue. For the default per-thread heap (`allow_page_abandon = true`) eviction =
**abandon** (move to the arena's `pages_abandoned[bin]` registry so other threads
can reclaim its memory). The queue then only ever holds pages that recently had
free space; the search is bounded by `page_full_retain` (2) + `page_max_candidates`
(4).

**Why eviction needs the delayed-free protocol.** Once a page is abandoned, frees
into it (cross-thread *or* by the original owner, who is no longer the page's
owner) must remain safe and must not let a now-empty abandoned page rot. v3 solves
this with the **ownership bit** in `xthread_free`'s LSB:

- A page managed by a live theap is *owned* (LSB = 1); cross-thread frees just push
  (LSB stays 1) and the owner collects later.
- An abandoned page is *not owned* (LSB = 0). The first free to push **claims** it
  (`fetch_or(1)` returns old-LSB 0 ⇒ "I now own it") and runs
  `mi_free_try_collect_mt`: collect → if all-free, *unabandon + free to arena*;
  else try to reclaim into our theap; else reabandon; else release ownership.

This is what prevents the V-round failure: V-round used a theap-**local** full
queue (the `allow_page_abandon = false` fallback path), so cross-thread frees
could never bring a full page back and abandoned-full pages accumulated →
phase-2 MT median +0.5→+8.0% regression (archived `archive/v-round-v1v2`). The
earlier P2 attempt abandoned full pages **without** the ownership protocol →
cross-thread abort 1/12→6/12. Source confirms there is **no shortcut**: faithful,
non-regressing full-page eviction requires the ownership-tagged `xthread_free`.

## Non-negotiable principles

1. **Faithful v3 port.** Mirror `free.c` (`mi_free_ex`, `mi_free_block_mt`,
   `mi_free_try_collect_mt`, `mi_abandoned_page_unown_from_free`),
   `page.c`/`page-queue.c` (`mi_page_to_full`, `_mi_page_unfull`,
   `mi_page_queue_find_free_ex`), the flag-folded `xthread_id` fast path, and the
   ownership helpers (`internal.h:919-950`). Same option names/defaults.
2. **Idiomatic Rust, Rust strengths.** Typed `ThreadFree(usize)` newtype for the
   `block | owned` word (no bare bit-twiddling at call sites); the 4-way free
   dispatch as a `match` on the XOR; ownership claim via `AtomicUsize::fetch_or`;
   reuse the existing `Bitmap` for the abandoned registry; `// SAFETY:` on every
   new unsafe; no panics on the hot path.
3. **Simple, clean code.** Reuse the arena meta-bitmap layout for
   `pages_abandoned[bin]` (extend the symmetric `[chunkmap][chunks]` block), reuse
   the `collect` heartbeat. Skip v3 micro-knobs (subproc reader-handshake for
   `pages_abandoned_mapped`, `page_cross_thread_max_reclaim` threadpool split)
   unless a measurement demands them — default `page_reclaim_on_free = 0`
   (reclaim only into the originating theap) is enough for the phase-1 win.

## ABSOLUTE bar (carried from prior rounds)

- **No performance regression** vs pre-round `main` on **any** `bench_suite`
  phase — eviction/abandon/claim must stay off the alloc/free fast path (the flag
  check is folded into the `xthread_id` XOR that free already does; **no per-alloc
  `is_full` cost** — that was the P2/V-round regression cause). Authoritative
  measurement on the user's pinned i7-14700K via `scripts/perf_compare.sh` (this
  env is too noisy). Merge a stage only if no regression. Phase-1 small **must
  improve** by FE2.
- Correctness locked at main's level (0): cross-thread TSan stress, loom (the
  ownership-claim race + push/drain), differential vs C v3.3.2 (all bins +
  edge + cross-thread), the secure+debug parallel abort gate, miri on new bitmap
  ops.
- Do **not** resurrect the local-full-queue cliff (`archive/v-round-v1v2`).
- "너무 힘들면 머지 안 해도 됨": any stage that cannot clear the no-regression bar is
  parked (archived as a tag) rather than forced through.

## v3 reference (cite when porting)

- **Flag-folded fast path:** `mi_free_ex` (`free.c:179-205`) — `xtid =
  tid ^ mi_page_xthread_id(page)`; 4 cases by `xtid` vs `MI_PAGE_FLAG_MASK`.
  Flags `MI_PAGE_IN_FULL_QUEUE=1`, `MI_PAGE_HAS_INTERIOR_POINTERS=2` live in the
  low 2 bits of `xthread_id` (`types.h`; `internal.h:860-891` set/get with
  flag-preserving CAS in `mi_page_set_theap`).
- **Ownership / delayed-free:** `internal.h:919-950` (`mi_tf_block/is_owned/
  create`, `mi_page_claim_ownership = atomic_or(…,1)`), `mi_free_block_mt`
  (`free.c:57-87`), `mi_free_try_collect_mt` (`free.c:357-390`),
  `mi_abandoned_page_unown_from_free` (`free.c:273-295`),
  `mi_abandoned_page_try_free/reclaim/reabandon` (`free.c:249-353`). Page init:
  `xthread_free == 1` (`page.c:721`).
- **Eviction:** `mi_page_to_full` (`page.c:382-397`) → `_mi_page_abandon`
  (`page.c:301-314`) when `allow_page_abandon`; `_mi_page_unfull`
  (`page.c:367-380`); the candidate/`page_full_retain` search
  `mi_page_queue_find_free_ex` (`page.c:744-854`); end-of-generic to-full
  (`page.c:1034-1037`). Queue flag maintenance: `page-queue.c:252-423`.
- **Abandoned registry:** per-arena `pages_abandoned[bin]` bitmaps; abandon =
  set slice bit + clear ownership; reclaim-on-alloc scans them; `unabandon`
  clears the bit (`arena.c`, `_mi_arenas_page_abandon/unabandon/alloc`).
- **Options / defaults (`options.c:165-167`):** `page_reclaim_on_free = 0`
  (-1 disable / 0 originating-theap-only / 1 cross-thread), `page_full_retain = 2`,
  `page_max_candidates = 4`. `allow_page_abandon = (page_full_retain >= 0)`
  (`init.c:309`).

## Current rs state (what exists / what's missing)

Exists: `Page.xthread_id: AtomicUsize` (`owner_tid` already masks
`MI_PAGE_FLAG_MASK`), flag constants in `bits.rs`
(`MI_PAGE_IN_FULL_QUEUE/HAS_INTERIOR_POINTERS/FLAG_MASK`,
`MI_THREADID_ABANDONED=0`, `MI_THREADID_ABANDONED_MAPPED=4`), a working
**lock-based** abandon registry (`Subproc::{abandon_page,reclaim_page}` over a
per-bin `SpinLock` stack + `Page::abandoned_next`), `xthread_free: AtomicPtr<Block>`
(plain Treiber, **no ownership LSB** — "follow-up work" per `page.rs:46-48`),
`collect()` owner-drain, `try_reclaim` on alloc, arena meta-bitmaps
(`free`/`commit`/`purge`, symmetric `[chunkmap][chunks]`).

Missing / divergent: no flag-folded XOR dispatch (free routes via a plain
`owner_tid == current_tid` compare, `heap.rs:507-536`); no `in_full`/`has_interior`
flag set/clear; `xthread_free` has no ownership bit, so a free into an abandoned
page cannot claim/collect/free it (the lock stack is pop-only — can't remove an
arbitrary page); no `mi_page_to_full`/`_mi_page_unfull`/candidate search — full
pages are never evicted from the bin queue (the bottleneck); no
`page_full_retain`/`page_max_candidates`/`page_reclaim_on_free` options.

## Milestones (low-risk → high-risk; one PR each, measured)

### FE0 — Flag-folded free fast path  *(low risk; behavior-preserving)*
- Add `Page` flag accessors over `xthread_id`'s low bits: `set_in_full(bool)` /
  `is_in_full()` and `set_has_interior()` / `has_interior()` via atomic or/and,
  and make `set_owner` preserve existing flag bits with a flag-preserving CAS
  (port `mi_page_set_theap`, `internal.h:867-871`). Add a raw
  `xthread_id_raw(page)` (unmasked) reader.
- Restructure `heap::free` block routing into v3's 4-way XOR `match`
  (`xtid = current_tid ^ xthread_id_raw`): `0` → fast local (no full/interior
  work); `≤ FLAG_MASK` → local generic (handle interior + later unfull);
  `& FLAG_MASK == 0` → cross-thread fast; else → cross-thread generic. The
  block-start fast path (`off == 0`) divide-skip is preserved for the `xtid == 0`
  case only; interior pointers are gated by `has_interior`.
- `alloc_aligned`: set `has_interior` on the page whenever it hands out an
  interior pointer (so a flags==0 page never receives one — keeps the fast-path
  divide-skip sound).
- **DoD:** in_full unused yet (always 0) ⇒ behavior identical; differential (all
  bins + edge + cross-thread) green, abort gate 0, loom push/drain unchanged,
  clippy/fmt; perf-neutral (the XOR replaces the compare — same or fewer ops).

### FE1 — Ownership-tagged `xthread_free` + collect-on-free + bitmap registry  *(HIGH risk — the concurrency core)*
- Replace `xthread_free: AtomicPtr<Block>` with an ownership-tagged
  `AtomicUsize` behind a typed `ThreadFree(usize)` (`block | owned`); helpers
  `tf_block/tf_is_owned/tf_create`, `claim_ownership = fetch_or(1) → was-unowned`.
  Page init sets it to `1` (owned, empty). `collect()` preserves the owned bit on
  the swap-to-empty.
- `thread_free_push` → `free_block_mt` (`free.c:57-87`): push with `owned=true`;
  if the prior tf was unowned, the page was abandoned and we just claimed it →
  `free_try_collect_mt`.
- Port `free_try_collect_mt` (`free.c:357-390`): collect, then in order — *try
  free* (all-free → `page_unabandon` + return slices to arena), *try reclaim*
  (only the originating theap by default, `page_reclaim_on_free`), *reabandon*,
  else `unown_from_free` (`free.c:273-295`, CAS the owned bit back off while
  draining any concurrent pushes).
- **Bitmap abandon registry:** add per-arena `pages_abandoned[MI_BIN_COUNT]`
  bitmaps (extend the meta-bitmap block; reuse `Bitmap`). `arena.page_abandon`
  sets the page's start-slice bit; `arena.page_unabandon` clears it;
  `subproc.reclaim_page(bin)` scans arenas, claims ownership, clears the bit.
  Remove the `SpinLock` stack + `Page::abandoned_next`. Thread-exit `Heap::drop`
  abandon and full-page abandon now share this one mechanism.
- **DoD (gated):** loom — the ownership-claim race (N concurrent frees into one
  abandoned page: exactly one claims & frees, none lost/duplicated) + push/drain;
  TSan — all 4 cross-thread tests + abandoned-reclaim; differential cross-thread;
  abort gate 0; miri on the registry bitmap ops; **no full-page eviction yet**
  (this only swaps the abandon mechanism, equivalent for thread-exit) ⇒ pinned
  perf_compare neutral.

### FE2 — Full-page eviction (`to_full` → abandon) + queue pruning  *(HIGH risk — the perf win)*
- Port the candidate search `mi_page_queue_find_free_ex` (`page.c:744-854`):
  next-fit with `page_max_candidates` (4), prefer fuller non-mostly-used
  candidates, and on a page that stays full decrement `page_full_retain` (2 for
  small, 0 for larger) → `page_to_full` → **abandon** (`allow_page_abandon =
  page_full_retain >= 0`, default true). End-of-generic: block_size >
  `MI_SMALL_MAX_OBJ_SIZE` & full → `to_full` (`page.c:1034-1037`).
- `_mi_page_unfull`: only reachable when `allow_page_abandon == false`; with the
  default-true heap, full pages are abandoned (not in a local full queue), so the
  in_full flag is set transiently for queue accounting only. Keep the
  `allow_page_abandon == false` local-full-queue + unfull path faithful for
  destroyable heaps (`Heap::destroy` already exists) but it is **off the default
  path**.
- Add options `page_reclaim_on_free=0`, `page_full_retain=2`,
  `page_max_candidates=4`.
- The `pages_free_direct` fast path is untouched; full detection on the free path
  is the FE0 flag (zero added cost).
- **DoD (gated):** differential + TSan + abort gate + miri stay 0; **pinned
  `perf_compare.sh`: phase-1 small improves and NO phase regresses** (the
  make-or-break gate). If MT regresses, FE1's collect-on-free should prevent the
  V-round rot — investigate before forcing; park if unresolved.

### FE3 — Verify + lock  *(measurement + docs)*
- Full pinned-machine `perf_compare.sh` across all phases (no regression + the
  phase-1 win); confirm RSS unaffected (`examples/rss_spike.rs`).
- Update `docs/perf-hotpath.md` §C1 (mark the full-page-eviction gap resolved),
  module docs, `CHANGELOG.md`, and the project memory.
- Final gate: ai-slop-cleaner + verifier/code-review (writer/reviewer separated).

## Concurrency hazards to get right (FE1)

- **Exactly-once claim.** Only the free whose `fetch_or(1)` observes old-LSB 0 may
  collect/free/unabandon the page; all others just push. Mirrors
  `mi_free_block_mt` + `mi_page_claim_ownership`.
- **Unabandon races the registry.** Clearing the `pages_abandoned[bin]` bit and
  returning slices must be ordered so no allocator reclaims a page mid-free.
  v3 uses a reader handshake for the *mapped* variant; for the simple bitmap
  registry, the ownership bit is the serialization point (a reclaimer must also
  claim ownership before unabandoning) — model this in loom.
- **used-count vs cross-thread free.** A cross-thread free never decrements
  `used`; only `collect` does. So "all-free" is only observable after a collect by
  the owning/claiming thread — `release_page_slices`'s existing happens-before
  argument (`heap.rs:614-630`) must be preserved through the ownership swap.
- **Owned-bit preservation.** Every `xthread_free` mutation (push, collect-swap,
  unown) must preserve/transition the LSB exactly as v3 does; a dropped bit
  abandons a live page or double-frees an abandoned one.

## Definition of done (round)

All FEs merged (or explicitly parked); phase-1 small no longer regresses (and
improves) with **zero regression on any other `bench_suite` phase (pinned
machine)**; loom + miri + TSan + differential + abort gates green; options match
v3 defaults; docs + memory updated.
