<!-- SPDX-License-Identifier: MIT -->
# GOAL — FE2 revival: close the heavy cross-thread-free gap (head-on)

> Round brief for the FE2-revival work. The fair LD_PRELOAD sweep proved the one
> remaining gap vs mimalloc-C is **heavy producer/consumer cross-thread free**:
> `xmalloc-test` **−24%** (rs 232 M vs C 306 M free/sec), `alloc-test` **+15%**
> (2.99 s vs 2.60 s). Every other workload is within ~4% (`docs/FIDELITY.md` §4).
> This round confronts the root cause directly — full-page eviction — which has
> been parked three times. It is the hard one; there is no easy detour.

## 1. Root cause (profiled, not guessed)

`xmalloc-test`: some threads allocate, *other* threads free. Pinned i7-14700K,
both LD_PRELOAD, `perf`:

- **mimalloc-rs**: 46% of time in `free`; **51% of that is a single
  `lock cmpxchg` on `xthread_free`** + `pause` spin-retries. Many consumers
  CAS-contend on the **live owner's** page; the owner's alloc then keeps hitting
  the cold collect/generic path (`alloc_generic` 21%, `Page::alloc_slow` 10%).
- **mimalloc-C**: `mi_free_try_collect_mt` 31% + `_mi_page_free_collect_partly`
  4.5% + `_mi_arenas_page_try_reabandon_to_mapped` 5% +
  `mi_abandoned_page_try_reclaim` 2.9%. C **abandons** the full page, a freeing
  thread **claims** it, and collects **locally without atomics** (`_partly`).

### Why there is no detour
`_partly` collect and reclaim-on-free live **only on the claim path of an
abandoned page** (`mi_free_try_collect_mt`). In the current port, pages are
abandoned **only at thread exit** (full-page eviction is parked), so during a
steady-state run *no page is ever abandoned*, `free_try_collect_mt` is **never
called**, and every cross-thread free just CAS-contends on the live owner.
**Therefore full-page eviction (abandon-on-full) is the prerequisite enabler** —
the cheaper claim-path pieces do nothing without it. We must take the hard piece
head-on.

## 2. Failure history (three parked attempts) — and what is different now

| Attempt | What it did | How it failed |
|---|---|---|
| **P2** (`feat/perf-full-queue-v3`, deleted) | evict full pages, **no** delayed-free/claim protocol | cross-thread frees into off-queue pages → **abort 1/12 → 6/12** (unsafe) |
| **V-round V2** (`archive/v-round-v1v2`) | lazy eviction "cliff" + lock-free abandoned Treiber | correct, but **perf_compare phase-2 (MT) +0.5→+8.0%** churn → parked |
| **FE2** (`archive/fe2-full-page-eviction`) | `find_free_page` + `page_full_retain=2` (small-only) + `abandon_owned_page` + `reclaim_on_free` (originating) | correct (loom/TSan green), but **perf_compare phase-4 (cross-thread) regressed**; net-negative → parked |

**What is genuinely different this time:**
1. **collect-O1 (#42) is now in main.** All three attempts predate it; the
   baseline collect cost (and thus the churn cost) is different now — must
   re-measure from scratch.
2. **We now have the FAIR LD_PRELOAD sweep** (`xmalloc-test`/`alloc-test`) — the
   *actual* beneficiary of eviction. The parked attempts were gated **only** on
   the in-repo `perf_compare` (which compares rs-vs-rs static, and whose phase-4
   is a *low-contention clean handoff* that eviction only churns). We were
   optimizing against a proxy that does not benefit; now we measure the workload
   that does.
3. **FE2 lacked `_mi_page_free_collect_partly`** — it always full-collected
   (atomic swap) on the claim path. Adding the no-atomic `_partly` collect is the
   missing efficiency piece that makes the claim path cheap.
4. **Safety net is complete**: ownership-LSB protocol, differential vs C,
   loom (claim-exactly-once), TSan — all already green and reusable.

## 3. The central tension (must be resolved by measurement)

The two cross-thread workloads respond **oppositely** to eviction:

- **High-contention many-consumers** (`xmalloc-test`): eviction **helps** — it
  moves the page to a freeing thread so frees become local, killing the
  contended CAS.
- **Low-contention clean handoff** (`perf_compare` phase-4: one producer → one
  consumer): eviction **hurts** — the page ping-pongs abandon↔reclaim with no
  contention to relieve, pure churn.

So a naïve always-evict is net-negative on phase-4 (the parked result). Levers to
balance: `page_full_retain` (keep N full pages before evicting — higher = less
churn), small-only eviction, the `_partly` collect (makes whatever churn remains
cheaper), and the originating-heap `reclaim_on_free` (a producer reclaiming its
own evicted page locally). The round's job is to find a setting that is
**net-positive on the fair sweep without regressing `perf_compare`** — or to
prove no such setting exists and quantify the tradeoff for an explicit decision.

## 4. Non-negotiable principles

1. **Faithful v3 port.** Mirror `mi_page_queue_find_free_ex` (page_full_retain,
   first-fit), `_mi_page_abandon`, `_mi_page_free_collect_partly`,
   `mi_abandoned_page_try_reclaim` semantics + option names/defaults
   (`page_full_retain=2`, `page_reclaim_on_free=0`).
2. **Idiomatic Rust, Rust strengths.** Reuse the ownership-LSB protocol and the
   `Cell`/`AtomicPtr` typing; `// SAFETY:` on every new unsafe; panic-free hot
   path. Keep the `cfg(override_export)` cold-split discipline.
3. **Simple, clean.** Reuse the archived FE2 code where sound; add only the
   missing `_partly` collect and the tuning. Don't port knobs that don't move a
   measured benchmark (note any omission).

## 5. ABSOLUTE bar & decision criteria

- **Authoritative measurement, by the assistant, on the pinned i7-14700K**, each
  stage:
  - **Fair sweep** (both LD_PRELOAD, interleaved median): `xmalloc-test`,
    `alloc-test` (targets) **must improve**; `larson`, `cfrac`, `espresso`,
    `mstress`, `rptest`, `malloc-large` **must not regress**.
  - **In-repo `scripts/perf_compare.sh 9 2`** (static, rs-vs-main): no phase
    regresses (phase-4 is the historical victim — watch it, but read min+median
    given its ±8% noise).
- **Merge criterion:** a stage merges only if the fair sweep nets positive AND
  `perf_compare` shows no regression. If the two conflict (sweep up, phase-4
  down), **stop and present the tradeoff to the user with numbers** — do not
  force it through, do not silently regress the static path.
- **Correctness locked at main's level:** `cargo test` (incl. cross-thread),
  hardened abort gate, differential vs C v3.3.2, loom, TSan, Miri — all green.
  New eviction invariants get focused tests (queue stays bounded; every block
  valid + freed without leak under heavy eviction; claim-exactly-once under
  concurrent free + reclaim).
- Anything that cannot clear the bar is parked (tagged + documented), not forced.

## 6. Staged plan (each an independently measured PR)

> Eviction is the prerequisite, so it lands first — but minimally and behind the
> `page_full_retain` damper — then the claim-path efficiency, then tuning.

### FR1 — Port `_mi_page_free_collect_partly` (no-atomic small collect)
- `Page::collect_partly(head)` (ports `page.c:243-261`): given the just-pushed
  `mt_free` head, collect the *rest* of the thread-free list into `local_free`
  **without** the atomic swap (the head is left for the owner), update `used`,
  splice into `free`. Use it from `free_try_collect_mt` for `block_size <=
  MI_SMALL_SIZE_MAX`; keep full-collect for larger.
- Standalone-correct even before eviction (just a cheaper collect on the claim
  path). DoD: differential + loom + TSan green; no perf change yet (claim path
  not yet hot without eviction).

### FR2 — Full-page eviction + reclaim-on-free (revive FE2 on current main)
- Re-apply the archived FE2 design onto post-collect-O1 main: `find_free_page`
  (first-fit + `page_full_retain` budget, small-only), `page_to_full` →
  `abandon_owned_page` (empty→release / full→unmapped / else→mapped),
  `reclaim_on_free` (originating-heap, hooked into `free_try_collect_mt` step 2,
  now using FR1's `_partly` collect). `PageFullRetain` option (default 2).
- DoD: the eviction invariant tests; **measure the fair sweep + perf_compare**.
  This is the make-or-break stage.

### FR3 — Tune the damper to resolve the tension
- Sweep `page_full_retain` (2 / 4 / 8 / 16) and small-only-vs-also-medium on the
  pinned machine; pick the setting that maximizes the fair-sweep win subject to
  `perf_compare` no-regression. If none exists, record the tradeoff and decide
  with the user (default-off option vs park).
- DoD: a chosen, justified default; full sweep + perf_compare evidence.

### FR4 — Verify + lock
- Full pinned sweep + `perf_compare` across all phases; update `FIDELITY.md` §4
  (close or quantify the gap), `PORTING-STATUS.md` (flip eviction/reclaim/`_partly`
  to ✅/🟡), module docs, memory. ai-slop-cleaner + verifier/code-review
  (writer/reviewer separated).

## 7. Definition of done (round)
Either: the fair sweep gap (`xmalloc-test`/`alloc-test`) is closed/narrowed with
**no `perf_compare` regression** and all correctness gates green, the changes
merged; **or** it is demonstrated (with pinned numbers) that closing it
necessarily regresses the static path, the tradeoff is presented, and the work is
parked behind a default-off option with the evidence documented. No silent
regression; no forcing through.
