// SPDX-License-Identifier: MIT
//! Pages (ports the `mi_page_t` core of `page.c` / `alloc.c` / `free.c`).
//!
//! A page owns a run of arena slices and serves fixed-size blocks from a sharded
//! free list. Its header lives inline at the start of its first slice; the
//! blocks follow at `page_start`.
//!
//! Three free lists (mimalloc's design):
//! * `free` — blocks `malloc` hands out (owner-only),
//! * `local_free` — blocks the owner freed, migrated to `free` on demand (keeps
//!   a monotonic heartbeat),
//! * `xthread_free` — blocks freed by *other* threads (atomic Treiber stack).
//!
//! The owner pops from `free`; other threads push to `xthread_free`; the owner
//! collects both `local_free` and `xthread_free` on demand. Pages carry their
//! owning heap/arena/bin so they can be retired or abandoned (thread exit).

use core::cell::Cell;
use core::ptr::NonNull;

use crate::atomic::{AtomicPtr, AtomicUsize, Ordering};
use crate::bits::{
    MI_ARENA_SLICE_SIZE, MI_INTPTR_SIZE, MI_MAX_ALIGN_SIZE, MI_PAGE_FLAG_MASK,
    MI_PAGE_HAS_INTERIOR_POINTERS, MI_PAGE_IN_FULL_QUEUE, MI_THREADID_ABANDONED_MAPPED,
};
use crate::free_list::Block;
use crate::layout::align_up;

// ---------------------------------------------------------------------------
// `xthread_free` ownership tag (ports v3's `mi_tf_*`, internal.h:919-950)
// ---------------------------------------------------------------------------
//
// The `xthread_free` head word is a `*mut Block` whose **low bit** is the page's
// ownership token: a page managed by a live theap (or temporarily claimed by a
// freeing thread) is *owned* (1); an abandoned page is *unowned* (0) until a
// free claims it. Blocks are at least pointer-aligned, so the low bit is always
// free. We tag through `map_addr`, which **preserves provenance**, so the head
// stays a real (deref-able) pointer with no `expose`/`with_exposed` round-trip.

/// Ownership bit of an `xthread_free` head word.
const TF_OWNED: usize = 1;

/// The block pointer of a thread-free head (low ownership bit masked off).
#[inline]
fn tf_block(tf: *mut Block) -> *mut Block {
    tf.map_addr(|a| a & !TF_OWNED)
}
/// Is the page owned (head word's low bit set)?
#[inline]
fn tf_is_owned(tf: *mut Block) -> bool {
    tf.addr() & TF_OWNED != 0
}
/// Build a head word from a block pointer and an ownership flag.
#[inline]
fn tf_create(block: *mut Block, owned: bool) -> *mut Block {
    block.map_addr(|a| (a & !TF_OWNED) | (owned as usize))
}

/// Extend the free list by at most this many bytes' worth of blocks at a time
/// (ports `MI_MAX_EXTEND_SIZE`); bounds the upfront init cost per batch.
const MI_MAX_EXTEND_SIZE: usize = 4096;
/// Always extend by at least this many blocks (ports `MI_MIN_EXTEND`).
///
/// Upstream uses `8*MI_SECURE` under `secure` to enlarge batches for the
/// randomized free-list-shuffle hardening; that shuffle path is not yet ported
/// (see the `secure` free-list work), so we keep `1` for all builds. This only
/// affects batch size, not correctness.
const MI_MIN_EXTEND: usize = 1;

/// A mimalloc page: header for a run of slices serving fixed-size blocks.
#[repr(C)]
pub struct Page {
    /// Owner thread id | page flags (0/4 if abandoned). Cross-thread visible.
    pub xthread_id: AtomicUsize,
    /// Owner free list (malloc pops from here).
    free: Cell<*mut Block>,
    /// Owner-deferred frees, migrated into `free` on demand.
    local_free: Cell<*mut Block>,
    /// Cross-thread free list: an atomic Treiber stack of blocks freed by other
    /// threads. Stored as `AtomicPtr` so pointer provenance is preserved. The
    /// ownership-bit tagging used for *abandoned* page reclaim is follow-up work.
    pub xthread_free: AtomicPtr<Block>,
    /// Blocks currently handed out (alive + in thread_free).
    used: Cell<u32>,
    /// Blocks threaded onto the free lists so far.
    capacity: Cell<u32>,
    /// Total blocks the page can ever hold.
    reserved: u32,
    /// Block size in bytes (const).
    block_size: usize,
    /// Start of the block area (const).
    page_start: *mut u8,
    /// Free-list encoding keys (const).
    keys: [usize; 2],
    /// Bin-queue links (owned by the heap).
    pub next: Cell<*mut Page>,
    pub prev: Cell<*mut Page>,
    /// Provenance of the page within its arena.
    pub slice_index: usize,
    pub slice_count: usize,
    /// Owning heap (set after init; owner-only access). Used to retire the page
    /// to its bin queue when it empties.
    heap: Cell<*mut crate::heap::Heap>,
    /// Owning arena (set after init). Slices return here on retire.
    arena: Cell<*mut crate::arena::Arena>,
    /// Size-class bin index this page belongs to (set after init).
    bin: Cell<u32>,
}

impl Page {
    /// Initialize a page in place at the start of its slice run.
    ///
    /// Lays the header at `slice_ptr`, places blocks after it, and threads the
    /// whole free list. Returns a pointer to the in-place header.
    ///
    /// # Safety
    /// `slice_ptr` must point at `slice_count` committed, slice-aligned slices
    /// that are not otherwise in use. `block_size >= MI_INTPTR_SIZE`.
    pub unsafe fn init(
        slice_ptr: NonNull<u8>,
        slice_index: usize,
        slice_count: usize,
        block_size: usize,
        keys: [usize; 2],
    ) -> NonNull<Page> {
        debug_assert!(block_size >= MI_INTPTR_SIZE);
        let region = slice_count * MI_ARENA_SLICE_SIZE;
        let header = align_up(core::mem::size_of::<Page>(), MI_MAX_ALIGN_SIZE);
        // Align the block area so blocks get natural alignment up to max-align.
        let start_off = align_up(header, MI_MAX_ALIGN_SIZE);
        let area = region - start_off;
        let reserved = (area / block_size) as u32;

        let hdr = slice_ptr.as_ptr() as *mut Page;
        // SAFETY: slice_ptr is committed and large enough for the header.
        let page_start = unsafe { slice_ptr.as_ptr().add(start_off) };
        // SAFETY: writing a freshly-typed header into committed memory.
        unsafe {
            hdr.write(Page {
                xthread_id: AtomicUsize::new(0),
                free: Cell::new(core::ptr::null_mut()),
                local_free: Cell::new(core::ptr::null_mut()),
                // Owned + empty (ports `page->xthread_free == 1` at init): a fresh
                // page is owned by the heap that created it. `without_provenance`
                // is correct here — the word carries no block pointer yet.
                xthread_free: AtomicPtr::new(core::ptr::without_provenance_mut(TF_OWNED)),
                used: Cell::new(0),
                // Lazily built: the free list starts empty and is extended in
                // batches on demand (see `extend_free`), so page creation does
                // not touch every block's memory upfront (better cache locality).
                capacity: Cell::new(0),
                reserved,
                block_size,
                page_start,
                keys,
                next: Cell::new(core::ptr::null_mut()),
                prev: Cell::new(core::ptr::null_mut()),
                slice_index,
                slice_count,
                heap: Cell::new(core::ptr::null_mut()),
                arena: Cell::new(core::ptr::null_mut()),
                bin: Cell::new(0),
            });
            // No upfront free-list build: the first `alloc` extends it.
            NonNull::new_unchecked(hdr)
        }
    }

    /// Extend the free list from uninitialized capacity, in a bounded batch
    /// (ports `mi_page_extend_free`). Threads blocks `[capacity, capacity+n)`
    /// onto `free` in ascending address order and advances `capacity`. Does
    /// nothing once `capacity == reserved`. This keeps page creation from
    /// touching every block upfront and keeps freshly-initialized blocks hot in
    /// cache near their first use.
    ///
    /// # Safety
    /// Owner-only; called when `free` is empty. `self`'s `Cell` fields are not
    /// touched by other threads.
    unsafe fn extend_free(&self) {
        let cap = self.capacity.get() as usize;
        let reserved = self.reserved as usize;
        if cap >= reserved {
            return;
        }
        // Batch ~MI_MAX_EXTEND_SIZE bytes of blocks at a time (at least
        // MI_MIN_EXTEND), capped by the remaining capacity. Simplified-equivalent
        // of upstream's `bsize >= MI_MAX_EXTEND_SIZE ? MI_MIN_EXTEND : .../bsize`
        // branch for `MI_MIN_EXTEND == 1` (block_size >= MI_INTPTR_SIZE, so no
        // divide-by-zero).
        let max_extend = (MI_MAX_EXTEND_SIZE / self.block_size).max(MI_MIN_EXTEND);
        let extend = (reserved - cap).min(max_extend);
        // Thread `[cap, cap+extend)` with the lowest index at the head, prepended
        // to the current free list (empty in practice on the owner path) —
        // sequential addresses for locality.
        let mut head = self.free.get();
        let mut i = cap + extend;
        while i > cap {
            i -= 1;
            // SAFETY: block `i < reserved` lies within the page area.
            let b = unsafe { self.page_start.add(i * self.block_size) } as *mut Block;
            // SAFETY: `b` is a valid, writable block slot.
            unsafe {
                (*b).set_next(head, self.keys);
            }
            head = b;
        }
        self.free.set(head);
        self.capacity.set((cap + extend) as u32);
    }

    /// Migrate `local_free` and the cross-thread `xthread_free` list into `free`.
    ///
    /// Cross-thread freed blocks were not counted against `used` by the freeing
    /// thread (only the owner mutates `used`), so we decrement `used` here.
    ///
    /// # Safety
    /// Owner-only; `self`'s `Cell` fields are not touched by other threads.
    unsafe fn collect(&self) {
        // 1. Drain the cross-thread free stack. Capture the list with a CAS that
        // resets the head to (NULL, owned) — **preserving the ownership bit** so a
        // concurrent freer keeps seeing the page as owned (ports
        // `mi_page_thread_free_collect`). Retry if a freer pushed meanwhile.
        loop {
            let tfree = self.xthread_free.load(Ordering::Acquire);
            let mut tf = tf_block(tfree);
            if tf.is_null() {
                break; // nothing queued; leave the ownership bit untouched
            }
            let empty = tf_create(core::ptr::null_mut(), tf_is_owned(tfree));
            if self
                .xthread_free
                .compare_exchange_weak(tfree, empty, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            // We now exclusively own the captured `tf` list; splice it into `free`.
            while !tf.is_null() {
                // SAFETY: `tf` blocks were published by freeing threads via
                // `set_next`, so the link is decoded the same way (encoded under
                // `secure`/`debug`).
                let next = unsafe { (*tf).next(self.keys) };
                // SAFETY: `tf` is a valid block slot owned by this page.
                unsafe {
                    (*tf).set_next(self.free.get(), self.keys);
                }
                self.free.set(tf);
                self.used.set(self.used.get() - 1);
                tf = next;
            }
            break;
        }
        // 2. Splice the owner-local deferred frees into `free`. Ports
        // `_mi_page_free_collect`'s key optimization: in the common case `free`
        // is empty, so move the whole `local_free` list over with a single
        // **O(1)** head assignment — *no traversal*. (The previous code walked and
        // re-linked every block, which made `Page::alloc` ~60% pointer-chasing the
        // `local_free` list on the single-thread fast path.) Only when `free`
        // already holds blocks (the cross-thread drain above prepended some) do we
        // walk `local_free` to its tail and append — the rare path.
        let lf = self.local_free.get();
        if !lf.is_null() {
            if self.free.get().is_null() {
                // Common: just adopt the list head (order preserved; links — encoded
                // under secure/debug — are already correct and untouched).
                self.free.set(lf);
            } else {
                // Rare: `free` is non-empty (xthread blocks drained in). Walk to
                // `local_free`'s tail and link it ahead of the current `free`.
                let mut tail = lf;
                loop {
                    // SAFETY: `tail` is a valid free block on our local list.
                    let next = unsafe { (*tail).next(self.keys) };
                    if next.is_null() {
                        break;
                    }
                    tail = next;
                }
                // SAFETY: `tail` is the last local_free block; link to current free.
                unsafe {
                    (*tail).set_next(self.free.get(), self.keys);
                }
                self.free.set(lf);
            }
            self.local_free.set(core::ptr::null_mut());
        }
    }

    /// Collect the cross-thread free list **without the atomic swap**, given the
    /// block `head` we just pushed onto `xthread_free` (now its head) when a free
    /// claimed an abandoned page. Ports `_mi_page_free_collect_partly`
    /// (`page.c:243`) — the no-atomic collect that keeps the cross-thread claim
    /// path cheap.
    ///
    /// We must not collect `head` itself: `xthread_free` still points at it and a
    /// concurrent freer may prepend a new block (writing *that* block's `next`,
    /// never `head`'s), so `head`'s own `next` is touched only by us. We sever
    /// `head` from the rest and migrate the rest (`head->next` onward) into the
    /// local lists with no atomic op; `head` stays queued and is picked up by a
    /// later full [`Page::collect`]. If only `head` remains live afterwards
    /// (`used == 1`), we full-collect to finish (the page is then empty).
    ///
    /// # Safety
    /// Owner (claim) path: the caller exclusively owns the page and `head` is the
    /// block it just pushed onto `xthread_free`.
    // Only reached from the std cross-thread claim path (`free_try_collect_mt`);
    // the no_std build is single-owner and never claims an abandoned page.
    #[cfg(feature = "std")]
    pub(crate) unsafe fn collect_partly(&self, head: *mut Block) {
        if head.is_null() {
            return;
        }
        // SAFETY: only the owner touches `head`'s `next`; concurrent pushers
        // prepend new blocks ahead of `head` and never touch `head`'s `next`.
        let next = unsafe { (*head).next(self.keys) };
        if !next.is_null() {
            // Sever `head` from the rest, then append the current `local_free`
            // after the captured list's tail and adopt the captured list as the
            // new `local_free` (ports `mi_page_thread_collect_to_local`), counting
            // blocks to correct `used` (the cross-thread freer never decremented).
            // SAFETY: `head` is ours; the `next` chain is now exclusively ours.
            unsafe { (*head).set_next(core::ptr::null_mut(), self.keys) };
            let mut count: u32 = 1;
            let mut last = next;
            loop {
                // SAFETY: walking the captured owner-only list.
                let n = unsafe { (*last).next(self.keys) };
                if n.is_null() {
                    break;
                }
                count += 1;
                last = n;
            }
            // SAFETY: `last` is the captured list's tail.
            unsafe { (*last).set_next(self.local_free.get(), self.keys) };
            self.local_free.set(next);
            self.used.set(self.used.get() - count);
            // Common case: `free` empty ⇒ adopt `local_free` wholesale (O(1)).
            if self.free.get().is_null() {
                self.free.set(self.local_free.get());
                self.local_free.set(core::ptr::null_mut());
            }
        }
        if self.used.get() == 1 {
            // Only `head` remains live ⇒ everything else was freed; full-collect
            // to grab `head` too (the page is then empty).
            // SAFETY: owner path.
            unsafe { self.collect() };
        }
    }

    /// Stamp the owning thread id on a **freshly initialized** page (flags are 0
    /// and the page is not yet published in the page-map, so no other thread can
    /// observe or mutate it) with a single plain store. This is the per-page
    /// creation path; on the huge-alloc workload (one page per allocation) the
    /// flag-preserving CAS below is a measurable per-op cost (a locked
    /// read-modify-write), so the fresh path must stay a plain store.
    #[inline]
    pub fn set_owner_fresh(&self, tid: usize) {
        debug_assert_eq!(tid & MI_PAGE_FLAG_MASK, 0, "tid must have clear flag bits");
        self.xthread_id.store(tid, Ordering::Release);
    }

    /// Restamp the owning thread id on an **already-live** page, **preserving the
    /// page flag bits** in the low `MI_PAGE_FLAG_MASK` bits (ports
    /// `mi_page_set_theap`'s flag-preserving CAS, `internal.h:867-871`). Used when
    /// reclaiming an abandoned page, which may carry `has_interior` from a prior
    /// life (and a concurrent thread may set it), so we must not clobber the flags.
    /// `tid` must have its low 2 bits clear (`current_tid` / `MI_THREADID_ABANDONED`
    /// both do).
    #[inline]
    pub fn set_owner(&self, tid: usize) {
        debug_assert_eq!(tid & MI_PAGE_FLAG_MASK, 0, "tid must have clear flag bits");
        let mut old = self.xthread_id.load(Ordering::Relaxed);
        loop {
            let new = tid | (old & MI_PAGE_FLAG_MASK);
            match self.xthread_id.compare_exchange_weak(
                old,
                new,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(e) => old = e,
            }
        }
    }

    /// Read the owning thread id (flags masked off) without forming a `&Page`
    /// (sound to call from a non-owner thread: only touches the atomic).
    ///
    /// # Safety
    /// `page` must point at a live page header.
    #[inline]
    pub unsafe fn owner_tid(page: *mut Page) -> usize {
        // SAFETY: project to the atomic field only (AtomicUsize: Sync).
        let xid = unsafe { &*core::ptr::addr_of!((*page).xthread_id) };
        xid.load(Ordering::Acquire) & !MI_PAGE_FLAG_MASK
    }

    /// Read the raw `xthread_id` (owner tid **with** the flag bits) without
    /// forming a `&Page`. This is what the free fast path XORs against the
    /// current thread id so the owner-vs-cross-thread decision and the
    /// full/interior flag check collapse into one compare (ports
    /// `mi_page_xthread_id` as used in `mi_free_ex`, `free.c:185`).
    ///
    /// # Safety
    /// `page` must point at a live page header.
    #[inline]
    pub unsafe fn xthread_id_raw(page: *mut Page) -> usize {
        // SAFETY: project to the atomic field only (AtomicUsize: Sync).
        let xid = unsafe { &*core::ptr::addr_of!((*page).xthread_id) };
        xid.load(Ordering::Acquire)
    }

    /// Set or clear the `in_full` flag (a page is in the heap's full queue). The
    /// flag lives in `xthread_id`'s low bits so the free fast path sees it for
    /// free; we use atomic or/and because a non-owner may concurrently set
    /// `has_interior`. Owner-only caller (queue accounting). Ports
    /// `mi_page_set_in_full`.
    #[inline]
    pub fn set_in_full(&self, in_full: bool) {
        if in_full {
            self.xthread_id
                .fetch_or(MI_PAGE_IN_FULL_QUEUE, Ordering::Release);
        } else {
            self.xthread_id
                .fetch_and(!MI_PAGE_IN_FULL_QUEUE, Ordering::Release);
        }
    }

    /// Is this page currently in the full queue?
    #[inline]
    pub fn is_in_full(&self) -> bool {
        self.xthread_id.load(Ordering::Acquire) & MI_PAGE_IN_FULL_QUEUE != 0
    }

    /// Mark that this page has handed out an interior pointer (from a
    /// large-alignment `alloc_aligned`), so the free fast path routes its
    /// pointers through the unalign (block-start recovery) path instead of
    /// assuming a block-start pointer. Set via atomic or — may be called from a
    /// non-owner. Ports `mi_page_set_has_interior_pointers` / the
    /// `MI_PAGE_HAS_INTERIOR_POINTERS` flag.
    #[inline]
    pub fn set_has_interior(&self) {
        self.xthread_id
            .fetch_or(MI_PAGE_HAS_INTERIOR_POINTERS, Ordering::Release);
    }

    /// Const block size, read via raw projection (immutable after init), so it
    /// is sound to read from a non-owner thread.
    ///
    /// # Safety
    /// `page` must point at a live page header.
    #[inline]
    pub unsafe fn raw_block_size(page: *mut Page) -> usize {
        // SAFETY: `block_size` is set once at init and never mutated.
        unsafe { core::ptr::read(core::ptr::addr_of!((*page).block_size)) }
    }

    /// Const block-area start, read via raw projection.
    ///
    /// # Safety
    /// `page` must point at a live page header.
    #[inline]
    pub unsafe fn raw_page_start(page: *mut Page) -> *mut u8 {
        // SAFETY: `page_start` is set once at init and never mutated.
        unsafe { core::ptr::read(core::ptr::addr_of!((*page).page_start)) }
    }

    /// Push `block` onto the page's cross-thread free stack (the freeing thread
    /// is *not* the owner), marking the head **owned**. Touches only the atomic
    /// head and the block's own memory, so it is sound to call concurrently with
    /// the owner. Ports `mi_free_block_mt`'s atomic push.
    ///
    /// Returns `true` if this push **claimed** the page — i.e. the head was
    /// *unowned* before (the page was abandoned) and is now owned by us, so the
    /// caller must run the collect-on-free protocol. Returns `false` if the page
    /// was already owned (a live or already-claimed page); then the owner will
    /// collect the block later.
    ///
    /// # Safety
    /// `page` is a live page header; `block` is a live block of that page no
    /// longer used by the caller.
    #[must_use]
    pub unsafe fn thread_free_push(page: *mut Page, block: NonNull<u8>) -> bool {
        // SAFETY: project to the atomic head and read the const keys only.
        let head = unsafe { &*core::ptr::addr_of!((*page).xthread_free) };
        let keys = unsafe { Page::raw_keys(page) };
        let bp = block.as_ptr() as *mut Block;
        // The next link is written through `Block::set_next`, so it shares the
        // owner free list's (optionally encoded) representation — under `secure`/
        // `debug` the cross-thread links are encoded too, not stored in the clear.
        // SAFETY: the block is exclusively ours until the CAS publishes it.
        let b = unsafe { &*(bp as *const Block) };
        loop {
            let cur = head.load(Ordering::Acquire);
            // SAFETY: link past the ownership bit to the real previous head.
            unsafe {
                b.set_next(tf_block(cur), keys);
            }
            // Always publish as owned: either the page was already owned (no-op on
            // the bit) or we are claiming a previously-abandoned page.
            let new = tf_create(bp, true);
            match head.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return !tf_is_owned(cur),
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    /// Const free-list keys, read via raw projection (immutable after init).
    ///
    /// # Safety
    /// `page` must point at a live page header.
    #[inline]
    pub unsafe fn raw_keys(page: *mut Page) -> [usize; 2] {
        // SAFETY: `keys` is set once at init and never mutated.
        unsafe { core::ptr::read(core::ptr::addr_of!((*page).keys)) }
    }

    /// (hardened builds) Is `block` already on this page's owner free lists?
    /// Used by [`crate::heap::free`] to detect double frees. Owner-only;
    /// `O(|free| + |local_free|)`, acceptable under `secure`/`debug`.
    #[cfg(any(feature = "secure", feature = "debug"))]
    pub fn owner_lists_contain(&self, block: *mut Block) -> bool {
        for mut p in [self.free.get(), self.local_free.get()] {
            while !p.is_null() {
                if core::ptr::eq(p, block) {
                    return true;
                }
                // SAFETY: `p` is a valid free block on an owner list.
                p = unsafe { (*p).next(self.keys) };
            }
        }
        false
    }

    /// Allocate one block, or `None` if the page is full.
    ///
    /// Fast path: pop the head of `free`. When `free` is empty the refill
    /// (collect cross-thread frees, then lazily extend) lives in [`Page::alloc_slow`],
    /// kept out of line **only in the preload cdylib** (`cfg(override_export)`) so
    /// this shell inlines across the export boundary — mirroring C's force-inlined
    /// `mi_page_malloc_zero` over the noinline generic refill. In a static build the
    /// optimizer folds `alloc_slow` back in (no forced call on the refill path).
    #[inline]
    pub fn alloc(&self) -> Option<NonNull<u8>> {
        let b = self.free.get();
        if b.is_null() {
            return self.alloc_slow();
        }
        // SAFETY: `b` is the current non-null free head.
        Some(unsafe { self.pop(b) })
    }

    /// Cold refill path: `free` was empty, so reclaim local/cross-thread frees
    /// and, if still empty, initialize the next batch of blocks on demand.
    /// (`#[cold]` only in the preload cdylib; see [`Page::alloc`].)
    #[cfg_attr(override_export, cold)]
    fn alloc_slow(&self) -> Option<NonNull<u8>> {
        // SAFETY: owner path — reclaim any local/cross-thread frees first.
        unsafe { self.collect() };
        let mut b = self.free.get();
        if b.is_null() {
            // Still empty: initialize the next batch of blocks on demand.
            // SAFETY: owner path; only runs when `free` is empty.
            unsafe { self.extend_free() };
            b = self.free.get();
            if b.is_null() {
                return None; // truly full: capacity == reserved
            }
        }
        // SAFETY: `b` is the current non-null free head.
        Some(unsafe { self.pop(b) })
    }

    /// Pop a known-non-null `free` head and return it as the allocated block.
    ///
    /// # Safety
    /// `b` must be the current (non-null) value of `self.free`.
    #[inline]
    unsafe fn pop(&self, b: *mut Block) -> NonNull<u8> {
        // Debug: every block handed out must lie on the page's block grid; a
        // violation means the free list was corrupted (e.g. an interior pointer
        // was pushed onto it).
        debug_assert_eq!(
            (b.addr() - self.page_start.addr()) % self.block_size,
            0,
            "free list corrupted: off-grid block {:#x}",
            b.addr()
        );
        // SAFETY: b is a valid free block; advance the free list.
        let next = unsafe { (*b).next(self.keys) };
        self.free.set(next);
        self.used.set(self.used.get() + 1);
        // SAFETY: b is within the page area and non-null.
        unsafe { NonNull::new_unchecked(b as *mut u8) }
    }

    /// Free a block back to this page (owner path).
    ///
    /// # Safety
    /// `p` must be a block previously returned by `self.alloc()`.
    pub unsafe fn free_local(&self, p: NonNull<u8>) {
        let b = p.as_ptr() as *mut Block;
        // SAFETY: b is a valid block slot in this page.
        unsafe {
            (*b).set_next(self.local_free.get(), self.keys);
        }
        self.local_free.set(b);
        // A live block always has `used > 0`; a violation here means a double
        // free or a foreign pointer (caller-contract violation).
        debug_assert!(self.used.get() > 0, "free of non-live block (double free?)");
        self.used.set(self.used.get().saturating_sub(1));
    }

    /// Record the owning heap, arena, and bin (called once after init, on the
    /// owner thread). Enables retiring the page when it empties.
    #[inline]
    pub fn set_provenance(
        &self,
        heap: *mut crate::heap::Heap,
        arena: *mut crate::arena::Arena,
        bin: u32,
    ) {
        self.heap.set(heap);
        self.arena.set(arena);
        self.bin.set(bin);
    }

    /// Owning heap pointer (owner-only).
    #[inline]
    pub fn owning_heap(&self) -> *mut crate::heap::Heap {
        self.heap.get()
    }

    /// Owning arena pointer.
    #[inline]
    pub fn owning_arena(&self) -> *mut crate::arena::Arena {
        self.arena.get()
    }

    /// Size-class bin index.
    #[inline]
    pub fn bin(&self) -> u32 {
        self.bin.get()
    }

    /// Migrate cross-thread + local frees into the `free` list (owner path).
    /// Called when adopting an abandoned page or before checking emptiness.
    ///
    /// # Safety
    /// Caller must own the page (no other thread runs the owner path).
    #[inline]
    pub unsafe fn collect_free(&self) {
        // SAFETY: forwarded owner-only contract.
        unsafe { self.collect() }
    }

    /// Is this page owned (the `xthread_free` ownership bit is set)?
    #[inline]
    pub fn is_owned(&self) -> bool {
        tf_is_owned(self.xthread_free.load(Ordering::Acquire))
    }

    /// Try to claim ownership of an abandoned page (set the ownership bit).
    /// Returns `true` if we transitioned unowned→owned (we now exclusively own
    /// it), `false` if it was already owned. Ports `mi_page_claim_ownership`
    /// (a CAS loop since stable `AtomicPtr` has no `fetch_or`). Sound to call
    /// from any thread — touches only the atomic head.
    #[inline]
    pub fn claim_ownership(&self) -> bool {
        loop {
            let cur = self.xthread_free.load(Ordering::Acquire);
            if tf_is_owned(cur) {
                return false;
            }
            let owned = tf_create(tf_block(cur), true);
            if self
                .xthread_free
                .compare_exchange_weak(cur, owned, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Release ownership of a page we hold, returning it to the unowned
    /// (abandoned) state. Expects the head to be `(NULL, owned)` — the state
    /// after a full `collect`. Returns `true` if it cleanly unowned; `false` if a
    /// concurrent freer pushed a block in the window (so the caller must
    /// re-collect and retry the collect-on-free ladder). Ports the CAS of
    /// `mi_abandoned_page_unown_from_free` (full-collect variant).
    ///
    /// # Safety
    /// Caller currently owns the page and has just collected it.
    #[inline]
    pub unsafe fn try_unown(&self) -> bool {
        let expect = tf_create(core::ptr::null_mut(), true);
        let newtf = tf_create(core::ptr::null_mut(), false);
        self.xthread_free
            .compare_exchange(expect, newtf, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Clear the ownership bit (hand the page off / abandon it), **preserving**
    /// any queued cross-thread block list. Unlike [`try_unown`], this is
    /// unconditional: it is used when a heap relinquishes a page at thread exit,
    /// where there is no collect-and-retry ladder — a block freed concurrently
    /// simply stays queued for whoever next claims the page. CAS-loop since stable
    /// `AtomicPtr` has no `fetch_and`.
    ///
    /// # Safety
    /// Caller currently owns the page and is giving it up.
    #[inline]
    pub unsafe fn set_unowned(&self) {
        loop {
            let cur = self.xthread_free.load(Ordering::Acquire);
            let unowned = tf_create(tf_block(cur), false);
            if self
                .xthread_free
                .compare_exchange_weak(cur, unowned, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Is this page abandoned *and mapped* (registered in its arena's
    /// `pages_abandoned[bin]`)? Encoded as `owner_tid == MI_THREADID_ABANDONED_MAPPED`.
    #[inline]
    pub fn is_abandoned_mapped(&self) -> bool {
        (self.xthread_id.load(Ordering::Acquire) & !MI_PAGE_FLAG_MASK)
            == MI_THREADID_ABANDONED_MAPPED
    }

    /// Block size served by this page.
    #[inline]
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Number of live (handed-out) blocks.
    #[inline]
    pub fn used(&self) -> u32 {
        self.used.get()
    }

    /// Does the page have a block ready to hand out *right now* (`free` is
    /// non-empty)? Cheap (no collect) — the page search checks this first and
    /// only collects on a miss (ports `mi_page_immediate_available`). Owner-only.
    #[inline]
    pub fn has_free(&self) -> bool {
        !self.free.get().is_null()
    }

    /// Can the page still initialize more blocks (capacity below reserved)? Such
    /// a page is not "full" — `alloc` will extend it on demand (ports
    /// `mi_page_is_expandable`). Owner-only.
    #[inline]
    pub fn is_expandable(&self) -> bool {
        self.capacity.get() < self.reserved
    }

    /// Is the page ≥7/8 used (few free slots left)? Ports `mi_page_is_mostly_used`
    /// — a page with plenty of free space (not mostly used) is worth taking over
    /// (cross-thread reclaim) so its frees become local; a mostly-used one is left
    /// to drain. Owner-only.
    #[inline]
    pub fn is_mostly_used(&self) -> bool {
        let frac = self.reserved / 8;
        self.reserved.saturating_sub(self.used.get()) <= frac
    }

    /// Total blocks the page can hold.
    #[inline]
    pub fn reserved(&self) -> u32 {
        self.reserved
    }

    /// Start of the block area.
    #[inline]
    pub fn page_start(&self) -> *mut u8 {
        self.page_start
    }

    /// Are all blocks free?
    #[inline]
    pub fn is_all_free(&self) -> bool {
        self.used.get() == 0
    }

    /// Is the page unable to serve another allocation? It is full only when the
    /// free list is empty after a collect **and** there is no uninitialized
    /// capacity left to extend (`capacity == reserved`); an extendable page can
    /// still serve, so it is not full.
    pub fn is_full(&self) -> bool {
        if !self.free.get().is_null() {
            return false;
        }
        // SAFETY: owner path.
        unsafe { self.collect() };
        if !self.free.get().is_null() {
            return false;
        }
        self.capacity.get() >= self.reserved
    }

    /// Does block-aligned `ptr` belong to this page's area?
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.page_start.addr();
        let a = ptr.addr();
        a >= base && a < base + (self.reserved as usize) * self.block_size
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::arena::Arena;

    fn with_page<R>(block_size: usize, f: impl FnOnce(&Page) -> R) -> R {
        // A single 64 KiB slice page.
        let arena = Arena::create(8, true).unwrap();
        // SAFETY: fresh arena.
        unsafe {
            let a = arena.as_ref();
            let (idx, p) = a.alloc_slices(1, 0).unwrap();
            let page = Page::init(p, idx, 1, block_size, [0x1234_5678, 0x9abc_def0]);
            let r = f(page.as_ref());
            a.free_slices(idx, 1);
            Arena::destroy(arena);
            r
        }
    }

    #[test]
    fn alloc_all_distinct_aligned_then_full() {
        with_page(64, |page| {
            let reserved = page.reserved() as usize;
            assert!(reserved > 100);
            let mut ptrs = alloc::vec::Vec::new();
            for _ in 0..reserved {
                let p = page.alloc().unwrap();
                assert_eq!(p.addr().get() % 16, 0, "block must be ≥16-aligned");
                assert!(page.contains(p.as_ptr()));
                ptrs.push(p.addr().get());
            }
            assert!(page.alloc().is_none(), "page should be full");
            assert_eq!(page.used() as usize, reserved);
            // all distinct and exactly block_size apart
            ptrs.sort_unstable();
            ptrs.dedup();
            assert_eq!(ptrs.len(), reserved);
            for w in ptrs.windows(2) {
                assert_eq!(w[1] - w[0], 64);
            }
        });
    }

    #[test]
    fn free_then_realloc_reuses_after_collect() {
        // mimalloc semantics: `local_free` blocks are NOT reused until `free`
        // is exhausted (a deferred-free "heartbeat"). So we drain the page,
        // free a few, and confirm the next allocations return exactly those.
        with_page(128, |page| {
            let reserved = page.reserved() as usize;
            // SAFETY: blocks come from this page.
            unsafe {
                let mut all = alloc::vec::Vec::new();
                for _ in 0..reserved {
                    all.push(page.alloc().unwrap());
                }
                assert!(page.alloc().is_none());
                // free three specific blocks
                let freed = [all[10], all[20], all[30]];
                for &p in &freed {
                    page.free_local(p);
                }
                assert_eq!(page.used() as usize, reserved - 3);
                // free was empty, so the next allocs collect local_free and must
                // return exactly the three freed blocks.
                let mut reused = alloc::vec::Vec::new();
                for _ in 0..3 {
                    reused.push(page.alloc().unwrap().addr().get());
                }
                assert_eq!(page.used() as usize, reserved);
                for &p in &freed {
                    assert!(
                        reused.contains(&p.addr().get()),
                        "freed block must be reused"
                    );
                }
                assert!(page.alloc().is_none(), "page full again");
            }
        });
    }

    #[test]
    fn write_blocks_no_overlap() {
        with_page(64, |page| {
            // SAFETY: blocks are within committed page memory.
            unsafe {
                let p0 = page.alloc().unwrap();
                let p1 = page.alloc().unwrap();
                core::ptr::write_bytes(p0.as_ptr(), 0x11, 64);
                core::ptr::write_bytes(p1.as_ptr(), 0x22, 64);
                // writing p1 must not have touched p0
                assert_eq!(*p0.as_ptr(), 0x11);
                assert_eq!(*p1.as_ptr(), 0x22);
            }
        });
    }

    /// Hardened-build double-free detection primitive: a freed block must be
    /// discoverable on the owner free lists (which is how `free` rejects a
    /// second free of the same pointer).
    #[cfg(any(feature = "secure", feature = "debug"))]
    #[test]
    fn detects_freed_block_for_double_free_guard() {
        with_page(64, |page| {
            // SAFETY: block comes from this page.
            unsafe {
                let a = page.alloc().unwrap();
                let ab = a.as_ptr() as *mut crate::free_list::Block;
                assert!(
                    !page.owner_lists_contain(ab),
                    "a live block is not on a free list"
                );
                page.free_local(a);
                assert!(
                    page.owner_lists_contain(ab),
                    "a freed block must be detectable (double-free guard)"
                );
            }
        });
    }

    extern crate alloc;
}

#[cfg(all(test, loom))]
mod loom_tests {
    //! Models the `xthread_free` Treiber stack protocol (push + swap-drain) in
    //! isolation — proving the contended CAS loop loses and duplicates nothing.
    extern crate alloc;
    use crate::atomic::{AtomicPtr, AtomicUsize, Ordering};
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::cell::UnsafeCell;

    struct Node {
        next: UnsafeCell<*mut Node>,
        id: usize,
    }
    // SAFETY: `next` is only written by the pushing thread before publish and
    // only read by the single draining thread after the swap — no concurrent
    // access to the cell itself; the head pointer carries the synchronization.
    unsafe impl Sync for Node {}
    unsafe impl Send for Node {}

    fn push(head: &AtomicPtr<Node>, node: *mut Node) {
        loop {
            let cur = head.load(Ordering::Acquire);
            // SAFETY: we exclusively own `node` until the CAS publishes it.
            unsafe {
                *(*node).next.get() = cur;
            }
            if head
                .compare_exchange_weak(cur, node, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    fn drain(head: &AtomicPtr<Node>) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = head.swap(core::ptr::null_mut(), Ordering::Acquire);
        while !cur.is_null() {
            // SAFETY: single consumer; node was published with release.
            let next = unsafe { *(*cur).next.get() };
            let id = unsafe { (*cur).id };
            out.push(id);
            cur = next;
        }
        out
    }

    #[test]
    fn contended_push_then_drain_no_loss_no_dup() {
        loom::model(|| {
            let head = Arc::new(AtomicPtr::<Node>::new(core::ptr::null_mut()));
            // two producers each push one distinct node
            let n0 = Box::into_raw(Box::new(Node {
                next: UnsafeCell::new(core::ptr::null_mut()),
                id: 1,
            }));
            let n1 = Box::into_raw(Box::new(Node {
                next: UnsafeCell::new(core::ptr::null_mut()),
                id: 2,
            }));
            let h0 = head.clone();
            let h1 = head.clone();
            let t0 = loom::thread::spawn(move || push(&h0, n0));
            let t1 = loom::thread::spawn(move || push(&h1, n1));
            t0.join().unwrap();
            t1.join().unwrap();

            let mut got = drain(&head);
            got.sort_unstable();
            assert_eq!(
                got,
                alloc::vec![1, 2],
                "lost or duplicated a cross-thread free"
            );

            // SAFETY: drained, no longer referenced.
            unsafe {
                drop(Box::from_raw(n0));
                drop(Box::from_raw(n1));
            }
        });
    }

    // Models the FE1b ownership-claim protocol abstractly: the `xthread_free`
    // head as `block<<1 | owned` (LSB = ownership token). These prove the
    // *exactly-once* claim invariant — the property that makes collect-on-free of
    // an abandoned page safe (only the single claimer frees/reabandons it).

    /// `thread_free_push` marking owned: CAS the head to `(block, owned=1)`;
    /// the push *claimed* the page iff the prior head was unowned.
    fn push_claim(head: &AtomicUsize, block: usize) -> bool {
        loop {
            let cur = head.load(Ordering::Acquire);
            let new = (block & !1) | 1;
            if head
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return cur & 1 == 0; // claimed iff it was unowned
            }
        }
    }

    /// `claim_ownership` (alloc-reclaim): set the LSB iff currently unowned.
    fn reclaim_claim(head: &AtomicUsize) -> bool {
        loop {
            let cur = head.load(Ordering::Acquire);
            if cur & 1 == 1 {
                return false; // already owned — give up
            }
            if head
                .compare_exchange_weak(cur, cur | 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    #[test]
    fn concurrent_frees_claim_abandoned_page_exactly_once() {
        loom::model(|| {
            // Abandoned page: unowned, empty head (0).
            let head = Arc::new(AtomicUsize::new(0));
            let claims = Arc::new(AtomicUsize::new(0));
            let mut hs = Vec::new();
            for block in [0b10usize, 0b100] {
                let h = head.clone();
                let c = claims.clone();
                hs.push(loom::thread::spawn(move || {
                    if push_claim(&h, block) {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            for h in hs {
                h.join().unwrap();
            }
            assert_eq!(
                claims.load(Ordering::Relaxed),
                1,
                "exactly one cross-thread free may claim the abandoned page"
            );
            assert_eq!(head.load(Ordering::Relaxed) & 1, 1, "page ends up owned");
        });
    }

    #[test]
    fn reclaim_races_free_claim_exactly_once() {
        loom::model(|| {
            // Abandoned page; an alloc-reclaimer and a cross-thread freer race.
            let head = Arc::new(AtomicUsize::new(0));
            let claims = Arc::new(AtomicUsize::new(0));
            let (h1, c1) = (head.clone(), claims.clone());
            let t_free = loom::thread::spawn(move || {
                if push_claim(&h1, 0b10) {
                    c1.fetch_add(1, Ordering::Relaxed);
                }
            });
            let (h2, c2) = (head.clone(), claims.clone());
            let t_reclaim = loom::thread::spawn(move || {
                if reclaim_claim(&h2) {
                    c2.fetch_add(1, Ordering::Relaxed);
                }
            });
            t_free.join().unwrap();
            t_reclaim.join().unwrap();
            assert_eq!(
                claims.load(Ordering::Relaxed),
                1,
                "exactly one of {{reclaim-on-alloc, free}} may claim the page"
            );
        });
    }
}
