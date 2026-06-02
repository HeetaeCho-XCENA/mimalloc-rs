<!-- SPDX-License-Identifier: MIT -->
# FE1 design preview — ownership-tagged `xthread_free` + collect-on-free + bitmap abandon registry

> Review-before-implement (per the round plan). FE1 is the concurrency-critical
> core: it replaces the port's plain `xthread_free` + lock-stack abandon with v3's
> ownership-LSB protocol + per-arena `pages_abandoned[bin]` bitmap registry, so a
> free into an abandoned page can safely claim, collect, and free/reclaim/reabandon
> it. This is the prerequisite that makes FE2's full-page eviction non-regressing.

## 1. What v3 actually does (confirmed from source)

**Ownership bit** (`internal.h:919-950`). `xthread_free` is a word `block | owned`:
- `tf_block(tf) = tf & ~1`, `tf_is_owned(tf) = tf & 1`, `tf_create(b,owned)`.
- `claim_ownership(page) = atomic_or(&xthread_free, 1)` → returns *true if it was
  not owned before* (we just claimed it). Page init: `xthread_free = 1` (owned, empty).
- A page managed by a live theap (in a bin queue) is **owned**. An abandoned page
  is **not owned** until a free claims it.

**Free of a non-owned block** (`mi_free_block_mt`, `free.c:57-87`): push `block`
with `owned=true`; if the *prior* tf was not owned, the page was abandoned and we
just transitioned it to owned → call `mi_free_try_collect_mt`.

**`mi_free_try_collect_mt`** (`free.c:357-390`), in order:
1. collect the thread-free list (updates `used`),
2. **try free**: if all-free → `_mi_arenas_page_unabandon` + `_mi_arenas_page_free`,
3. **try reclaim** (small/medium, `page_reclaim_on_free >= 0`): reclaim into the
   originating theap (default `page_reclaim_on_free = 0` ⇒ *only* the theap the
   page came from; `1` ⇒ cross-thread too),
4. **try reabandon-to-mapped**: if the page became "not mostly used" (has free
   space again) and is not already in the bitmap → add it,
5. else **unown** (`mi_abandoned_page_unown_from_free`, `free.c:273-295`): CAS the
   owned bit back off, draining any concurrent pushes (retrying free/reabandon if
   the list grew).

**Abandon registry — two states** (`arena.c:1113-1235`, the key subtlety):
- `pages_abandoned[bin]` is a **per-arena bitmap indexed by the page's start
  slice**. A set bit = page is *abandoned **and** mapped* (i.e. has free space,
  findable for allocation). Tagged in `xthread_id` as `MI_THREADID_ABANDONED_MAPPED`
  (= 4).
- **A full (or singleton) page is NOT put in the bitmap** (`_mi_arenas_page_abandon`
  only maps `!mi_page_is_full(page)`). A full abandoned page is just *unowned*
  (`xthread_id = MI_THREADID_ABANDONED = 0`, LSB cleared), invisible to the alloc
  search. It is resurrected only by a free into it (claim → collect_mt → if it is
  no longer full, `try_reabandon_to_mapped` adds it to the bitmap; if all-free,
  free it). This is why eviction needs the ownership protocol: full pages float
  unmapped and come back through free-claim.

**Reclaim-on-alloc** (`mi_arenas_page_try_find_abandoned`, `arena.c:692-744`):
scan `pages_abandoned[bin]` with `mi_bitmap_try_find_and_claim` — find a set bit,
run a callback that **claims page ownership (LSB) first**, and clear the bit only
if the claim wins. So ownership is the single serialization point between an
alloc-reclaimer and a concurrent free.

**Unabandon** (`_mi_arenas_page_unabandon`, `arena.c:1191`): the caller already
owns the page; clear the bitmap bit (`mi_bitmap_clear_once_set` busy-waits a
concurrent find-and-claim reader) and clear the mapped flag.

## 2. Current rs state being replaced

- `Page.xthread_free: AtomicPtr<Block>` — plain Treiber, **no owned bit**.
- `Subproc.abandoned[bin]: SpinLock`-protected singly-linked stack via
  `Page.abandoned_next`; `abandon_page` (push) / `reclaim_page` (pop). Reclaim is
  driven **only on alloc** (`Heap::try_reclaim`). A free into an abandoned page
  just pushes to `xthread_free` and the page waits in the stack — it can never be
  freed/reabandoned by a free (the rot that breaks full-page eviction).
- A singly-linked lock stack also cannot remove an *arbitrary* page (only pop), so
  it cannot support free-triggered free/reabandon even with a lock.

## 3. Target rs design

### 3a. Ownership-tagged `xthread_free`
- `Page.xthread_free: AtomicUsize` holding `block_ptr | owned`. A small typed
  helper module (or `ThreadFree(usize)` newtype) with `tf_block`/`tf_is_owned`/
  `tf_create`/`claim_ownership(fetch_or(1))`. Init = `1`.
- `thread_free_push` → `free_block_mt(page, block)`: CAS-push with `owned=true`;
  if prior tf was unowned, call `free_try_collect_mt(page)`.
- `collect()` (owner drain): swap to `tf_create(NULL, was_owned)` — **preserve the
  owned bit** (today it swaps to a bare null pointer).
- Block link encoding (`secure`/`debug`) is unchanged — only the head word gains
  the LSB tag; `tf_block` masks it before deref.

### 3b. Per-arena bitmap registry
- `Arena`: add `pages_abandoned[MI_BIN_COUNT]` bitmaps (extend the meta-bitmap
  block; reuse `Bitmap`). Memory ≈ `MI_BIN_COUNT × (1 + chunk_count)` BChunks per
  arena (~43 KB for a 4096-slice arena) — metadata, acceptable.
- New `Bitmap::try_find_and_claim(tseq, claim: impl Fn(usize) -> bool) -> Option<usize>`
  (find a set bit, run `claim`, clear only if it returns true) — ports
  `mi_bitmap_try_find_and_claim`. `clear(idx)` already exists for unabandon (the
  freeing thread owns the page, so a plain `clear` suffices — ownership is the
  gate; **validated in loom**, see §5).
- `Arena::page_abandon_mapped(page, bin)` = `set(slice_index)`; `page_unabandon`
  = `clear(slice_index)`.

### 3c. Abandon / collect-on-free / reclaim wiring
- `page_abandon(page, theap)`: collect; if all-free → free to arena; else if
  arena page **and not full** → set mapped bit + `set_owner(ABANDONED_MAPPED)` +
  unown; else (full/singleton) → `set_owner(ABANDONED)` + unown (no bitmap entry).
- `free_try_collect_mt(page)`: the §1 ladder (free / reclaim-originating /
  reabandon-to-mapped / unown).
- `Heap::try_reclaim(bin)` (alloc path): `arena.try_find_and_claim(bin, claim_ownership)`
  across arenas → on hit, unmap bit already cleared, `collect_free`, re-home.
- `Heap::drop` (thread exit): per page — empty → free; else `page_abandon`
  (mapped if not full, unowned if full). Replaces the lock-stack push.
- **Remove** `Subproc.abandoned`, `AbandonedBin`, `Page.abandoned_next`,
  `abandon_page`/`reclaim_page`.

### 3d. `MI_THREADID_ABANDONED_MAPPED` state
Use `owner_tid == 4` (mapped) vs `0` (plain abandoned); `is_abandoned = owner_tid <= 4`.
`set_owner` already preserves the low flag bits; the abandoned-state value (0/4)
is the high part, set explicitly at abandon/reclaim.

## 4. Proposed sub-staging (each its own PR + gate)

The ownership LSB and the bitmap registry are coupled (a free that frees an
abandoned page must remove it from the registry O(1), which the lock stack can't
do), so a clean split is along *capability*, not *data structure*:

- **FE1a — bitmap registry swap (no new free behavior).** Replace the lock-stack
  with the per-arena `pages_abandoned[bin]` bitmaps; abandon-on-exit and
  reclaim-on-alloc only (today's triggers). Full abandoned pages still need a home
  → keep them mapped too *for this stage* (a reclaimer that finds a full page just
  re-frees/skips it), OR keep the lock stack only for the full case. **Decision
  needed** (see §6). Gate: TSan 4 cross-thread tests + differential, perf-neutral.
- **FE1b — ownership LSB + collect-on-free.** Add the owned bit, `free_block_mt`
  claim, `free_try_collect_mt`, and the mapped/unmapped split (full pages leave
  the registry, return via free-claim). Gate: loom (claim race) + TSan + differential.

Alternatively **FE1 as one PR** (faithful, cohesive, but a larger review surface).
Recommendation: **two PRs (FE1a then FE1b)** for reviewability and a smaller
blast radius per gate.

## 5. loom / verification plan
- **loom — claim race:** N threads free into one abandoned page; exactly one
  `claim_ownership` wins and runs collect_mt; others push and return. No lost/dup
  frees; the page is freed exactly once.
- **loom — reclaim vs free:** an alloc-reclaimer (`try_find_and_claim`) races a
  free (`free_block_mt` claim) on the same mapped page; exactly one owns it; the
  bitmap bit ends consistent (cleared iff reclaimed/freed, set iff reabandoned).
  This validates "plain `clear` instead of v3's `clear_once_set` busy-wait".
- **loom — push/drain** (existing) still holds with the tagged head.
- **TSan:** the existing 4 cross-thread tests + abandoned-reclaim, plus a
  producer/consumer that drives full-page abandon→free-claim (added in FE2).
- **differential** (cross-thread) + **secure/debug abort gate** stay 0; **miri**
  on the new bitmap `try_find_and_claim`.
- **Pinned `perf_compare`** neutral for FE1 (no eviction yet); the make-or-break
  perf gate is FE2.

## 6. Open decisions for review
1. **Sub-staging:** FE1a+FE1b (recommended) vs one FE1 PR?
2. **`clear_once_set`:** rely on ownership-as-gate + plain `clear` (simpler, to be
   loom-proven) vs port v3's busy-wait reader-handshake faithfully? (Recommend the
   former if loom proves it equivalent — simpler, still correct.)
3. **`MI_THREADID_ABANDONED_MAPPED` distinction:** port the full mapped/unmapped
   split now (faithful; full pages off the search) vs keep all abandoned pages in
   the bitmap initially (simpler; a reclaimer may find a full page and skip it —
   slightly less efficient but correct)? (Recommend faithful split, since FE2's
   whole point is keeping full pages out of the search.)
4. **`page_reclaim_on_free` default 0** (reclaim only into the originating theap)
   — confirm we skip the threadpool/cross-thread-reclaim knobs for now.
