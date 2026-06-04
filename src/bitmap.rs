// SPDX-License-Identifier: MIT
//! Concurrent atomic bitmap (ports `src/bitmap.c` / `bitmap.h`).
//!
//! Layout, bottom-up: **bfield** (one `usize`), **[`BChunk`]** (512 bits over 8
//! cache-aligned fields), **chunkmap** (one [`BChunk`], bit *c* set iff chunk *c*
//! may be non-empty), **[`Bitmap`]** (a chunkmap + `N` chunks, borrowed view).
//!
//! Convention (matching v3 arenas): for the free-slices bitmap a **set bit means
//! free** — allocation is find-and-clear, freeing is set. Multi-field ranges
//! clear field-by-field with rollback (no half-claimed run).
//!
//! Not yet ported: the binned `mi_bbitmap_t` and SIMD field scanning.

use crate::atomic::{cas_weak_acq_rel, load_acquire, load_relaxed, AtomicUsize};
use crate::bits::{MI_BCHUNK_BITS, MI_SIZE_BITS};

/// Bits per field word (64 on 64-bit).
pub const FIELD_BITS: usize = MI_SIZE_BITS;
/// Bits per chunk (512).
pub const CHUNK_BITS: usize = MI_BCHUNK_BITS;
/// Fields per chunk (8).
pub const CHUNK_FIELDS: usize = CHUNK_BITS / FIELD_BITS;

#[inline]
const fn mask_n(n: usize) -> usize {
    debug_assert!(n <= FIELD_BITS);
    if n == FIELD_BITS {
        usize::MAX
    } else {
        (1usize << n) - 1
    }
}

// ---------------------------------------------------------------------------
// bfield-level atomic primitives (operate on a single `usize` word)
// ---------------------------------------------------------------------------

/// All `mask` bits currently set?
#[inline]
fn bf_all_set(a: &AtomicUsize, mask: usize) -> bool {
    (load_acquire(a) & mask) == mask
}

/// All `mask` bits currently clear?
#[inline]
fn bf_all_clear(a: &AtomicUsize, mask: usize) -> bool {
    (load_acquire(a) & mask) == 0
}

/// Set every bit in `mask` (idempotent for already-set bits). Returns true if
/// at least one bit transitioned 0→1.
fn bf_set_mask(a: &AtomicUsize, mask: usize) -> bool {
    let mut old = load_relaxed(a);
    loop {
        let new = old | mask;
        if new == old {
            return false;
        }
        match cas_weak_acq_rel(a, old, new) {
            Ok(_) => return true,
            Err(seen) => old = seen,
        }
    }
}

/// Clear all bits in `mask`, but only if they are *all* currently set.
/// Returns true on success (all were set and are now clear).
fn bf_try_clear_mask(a: &AtomicUsize, mask: usize) -> bool {
    let mut old = load_relaxed(a);
    loop {
        if (old & mask) != mask {
            return false; // not all set
        }
        let new = old & !mask;
        match cas_weak_acq_rel(a, old, new) {
            Ok(_) => return true,
            Err(seen) => old = seen,
        }
    }
}

/// Find `n` contiguous set bits within the word and atomically clear them.
/// Returns the starting in-word bit index on success.
fn bf_find_and_clear_run(a: &AtomicUsize, n: usize) -> Option<usize> {
    if n == 0 || n > FIELD_BITS {
        return None;
    }
    let m = mask_n(n);
    let mut old = load_relaxed(a);
    loop {
        // Find a shift where `n` contiguous bits are set.
        let mut shift = 0usize;
        let mut found = None;
        while shift + n <= FIELD_BITS {
            let probe = m << shift;
            if (old & probe) == probe {
                found = Some(shift);
                break;
            }
            // Skip ahead past the first clear bit inside the window.
            shift += 1;
        }
        let shift = found?;
        let probe = m << shift;
        let new = old & !probe;
        match cas_weak_acq_rel(a, old, new) {
            Ok(_) => return Some(shift),
            Err(seen) => old = seen, // contention: re-scan with the new value
        }
    }
}

// ---------------------------------------------------------------------------
// BChunk: 512 bits across 8 fields
// ---------------------------------------------------------------------------

/// A 512-bit cache-aligned chunk of the bitmap.
#[repr(C, align(64))]
pub struct BChunk {
    fields: [AtomicUsize; CHUNK_FIELDS],
}

#[inline]
const fn split(cbit: usize) -> (usize, usize) {
    (cbit / FIELD_BITS, cbit % FIELD_BITS)
}

impl BChunk {
    /// A zeroed chunk (all bits clear).
    pub fn zeroed() -> Self {
        BChunk {
            fields: core::array::from_fn(|_| AtomicUsize::new(0)),
        }
    }

    #[inline]
    fn field(&self, fidx: usize) -> &AtomicUsize {
        &self.fields[fidx]
    }

    /// Is the single bit `cbit` set?
    pub fn is_set(&self, cbit: usize) -> bool {
        let (f, b) = split(cbit);
        bf_all_set(self.field(f), 1 << b)
    }

    /// Are all `n` bits starting at `cbit` set? (may span fields, within chunk)
    pub fn is_set_n(&self, cbit: usize, n: usize) -> bool {
        self.for_each_field_mask(cbit, n, bf_all_set)
    }

    /// Are all `n` bits starting at `cbit` clear?
    pub fn is_clear_n(&self, cbit: usize, n: usize) -> bool {
        self.for_each_field_mask(cbit, n, bf_all_clear)
    }

    /// Apply `pred` to each (field, in-field mask) covering `[cbit, cbit+n)`,
    /// returning true only if `pred` holds for all of them.
    fn for_each_field_mask(
        &self,
        cbit: usize,
        n: usize,
        pred: impl Fn(&AtomicUsize, usize) -> bool,
    ) -> bool {
        debug_assert!(cbit + n <= CHUNK_BITS);
        let mut remaining = n;
        let mut bit = cbit;
        while remaining > 0 {
            let (f, b) = split(bit);
            let here = core::cmp::min(remaining, FIELD_BITS - b);
            let mask = mask_n(here) << b;
            if !pred(self.field(f), mask) {
                return false;
            }
            bit += here;
            remaining -= here;
        }
        true
    }

    /// Set `n` bits starting at `cbit` (mark free). Atomic per field.
    pub fn set_n(&self, cbit: usize, n: usize) {
        debug_assert!(cbit + n <= CHUNK_BITS);
        let mut remaining = n;
        let mut bit = cbit;
        while remaining > 0 {
            let (f, b) = split(bit);
            let here = core::cmp::min(remaining, FIELD_BITS - b);
            bf_set_mask(self.field(f), mask_n(here) << b);
            bit += here;
            remaining -= here;
        }
    }

    /// Non-atomic set of `n` bits (only safe before publication).
    ///
    /// # Safety
    /// No other thread may access this chunk concurrently.
    pub unsafe fn unsafe_set_n(&self, cbit: usize, n: usize) {
        // We still go through atomics but with relaxed ops; correctness only
        // requires single-threaded use here.
        self.set_n(cbit, n);
    }

    /// Atomically clear `n` bits starting at `cbit`, only if all are set.
    /// Rolls back on partial failure. Returns true on success.
    pub fn try_clear_n(&self, cbit: usize, n: usize) -> bool {
        debug_assert!(cbit + n <= CHUNK_BITS);
        // Single field fast path.
        let (f0, b0) = split(cbit);
        if b0 + n <= FIELD_BITS {
            return bf_try_clear_mask(self.field(f0), mask_n(n) << b0);
        }
        // Multi-field: clear in order, remembering masks for rollback.
        let mut cleared: [(usize, usize); CHUNK_FIELDS] = [(0, 0); CHUNK_FIELDS];
        let mut count = 0usize;
        let mut remaining = n;
        let mut bit = cbit;
        let mut ok = true;
        while remaining > 0 {
            let (f, b) = split(bit);
            let here = core::cmp::min(remaining, FIELD_BITS - b);
            let mask = mask_n(here) << b;
            if bf_try_clear_mask(self.field(f), mask) {
                cleared[count] = (f, mask);
                count += 1;
            } else {
                ok = false;
                break;
            }
            bit += here;
            remaining -= here;
        }
        if !ok {
            // Roll back: set the bits we cleared back to free.
            for &(f, mask) in &cleared[..count] {
                bf_set_mask(self.field(f), mask);
            }
            return false;
        }
        true
    }

    /// Find `n` contiguous set bits within this chunk and clear them.
    /// Returns the chunk-local start index.
    pub fn find_and_clear_n(&self, n: usize) -> Option<usize> {
        if n == 0 || n > CHUNK_BITS {
            return None;
        }
        if n <= FIELD_BITS {
            // Try within each field first (no cross-field run needed if it fits).
            for f in 0..CHUNK_FIELDS {
                if let Some(b) = bf_find_and_clear_run(self.field(f), n) {
                    return Some(f * FIELD_BITS + b);
                }
            }
            // A run of n<=64 could still straddle a field boundary; fall through
            // to the general scan to catch that case.
        }
        self.find_and_clear_n_spanning(n)
    }

    /// General contiguous run search across field boundaries within the chunk.
    fn find_and_clear_n_spanning(&self, n: usize) -> Option<usize> {
        let max_bits = CHUNK_BITS;
        let mut start = 0usize;
        while start + n <= max_bits {
            if self.is_set_n(start, n) {
                if self.try_clear_n(start, n) {
                    return Some(start);
                }
                // Lost a race; re-probe from the same start.
                continue;
            }
            start += 1;
        }
        None
    }

    /// Is the entire chunk clear?
    pub fn is_all_clear(&self) -> bool {
        self.fields.iter().all(|a| load_acquire(a) == 0)
    }

    /// Count set bits in the chunk.
    pub fn popcount(&self) -> usize {
        self.fields
            .iter()
            .map(|a| load_relaxed(a).count_ones() as usize)
            .sum()
    }
}

// ---------------------------------------------------------------------------
// Bitmap: chunkmap + N chunks
// ---------------------------------------------------------------------------

/// A borrowed view of an atomic bitmap: a chunkmap and its chunks.
///
/// The storage may live in arena-managed memory (cast from zeroed bytes — sound
/// because a zero word is a valid `AtomicUsize(0)`) or, in tests, a `Vec`.
pub struct Bitmap<'a> {
    chunkmap: &'a BChunk,
    chunks: &'a [BChunk],
}

impl<'a> Bitmap<'a> {
    /// Build a bitmap view from a chunkmap and chunk slice.
    ///
    /// `chunks.len()` must be in `1..=CHUNK_BITS` (the chunkmap holds one bit
    /// per chunk).
    pub fn from_parts(chunkmap: &'a BChunk, chunks: &'a [BChunk]) -> Self {
        debug_assert!(!chunks.is_empty() && chunks.len() <= CHUNK_BITS);
        Bitmap { chunkmap, chunks }
    }

    /// Number of chunks.
    #[inline]
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Maximum representable bits.
    #[inline]
    pub fn max_bits(&self) -> usize {
        self.chunks.len() * CHUNK_BITS
    }

    #[inline]
    fn chunk_of(&self, idx: usize) -> (usize, usize) {
        (idx / CHUNK_BITS, idx % CHUNK_BITS)
    }

    /// Mark the chunkmap bit for `cidx` as (potentially) non-empty.
    #[inline]
    fn chunkmap_set(&self, cidx: usize) {
        let (f, b) = split(cidx);
        bf_set_mask(self.chunkmap.field(f), 1 << b);
    }

    /// After clearing bits in chunk `cidx`, drop its chunkmap bit if the chunk
    /// is now empty (with the re-check protocol from `bitmap.c`).
    fn chunkmap_clear_if_empty(&self, cidx: usize) {
        if !self.chunks[cidx].is_all_clear() {
            return;
        }
        let (f, b) = split(cidx);
        // Clear the chunkmap bit, then re-check: if the chunk became non-empty
        // due to a concurrent set, restore the chunkmap bit.
        if bf_try_clear_mask(self.chunkmap.field(f), 1 << b) && !self.chunks[cidx].is_all_clear() {
            bf_set_mask(self.chunkmap.field(f), 1 << b);
        }
    }

    /// Set a single bit (mark free). Returns true if it transitioned 0→1.
    pub fn set(&self, idx: usize) -> bool {
        self.set_n(idx, 1)
    }

    /// Clear a single bit (mark allocated). Returns true if it was set.
    pub fn clear(&self, idx: usize) -> bool {
        self.clear_n(idx, 1)
    }

    /// Is bit `idx` set?
    pub fn is_set(&self, idx: usize) -> bool {
        let (c, b) = self.chunk_of(idx);
        self.chunks[c].is_set(b)
    }

    /// Set `n` bits starting at `idx` (range stays within one chunk).
    pub fn set_n(&self, idx: usize, n: usize) -> bool {
        let (c, b) = self.chunk_of(idx);
        debug_assert!(b + n <= CHUNK_BITS, "set_n range crosses a chunk boundary");
        let before_clear = self.chunks[c].is_clear_n(b, n);
        self.chunks[c].set_n(b, n);
        self.chunkmap_set(c);
        before_clear
    }

    /// Non-atomic bulk set used to initialize a bitmap (e.g. "all free").
    ///
    /// # Safety
    /// Must be called before the bitmap is shared with other threads.
    pub unsafe fn unsafe_set_n(&self, idx: usize, n: usize) {
        let mut remaining = n;
        let mut bit = idx;
        while remaining > 0 {
            let (c, b) = self.chunk_of(bit);
            let here = core::cmp::min(remaining, CHUNK_BITS - b);
            // SAFETY: forwarded single-threaded contract.
            unsafe {
                self.chunks[c].unsafe_set_n(b, here);
            }
            self.chunkmap_set(c);
            bit += here;
            remaining -= here;
        }
    }

    /// Clear `n` bits starting at `idx` (range stays within one chunk). Returns
    /// true if all were set.
    pub fn clear_n(&self, idx: usize, n: usize) -> bool {
        let (c, b) = self.chunk_of(idx);
        debug_assert!(
            b + n <= CHUNK_BITS,
            "clear_n range crosses a chunk boundary"
        );
        let ok = self.chunks[c].try_clear_n(b, n);
        if ok {
            self.chunkmap_clear_if_empty(c);
        }
        ok
    }

    /// Are all `n` bits starting at `idx` set?
    pub fn is_set_n(&self, idx: usize, n: usize) -> bool {
        let (c, b) = self.chunk_of(idx);
        if b + n > CHUNK_BITS {
            return false;
        }
        self.chunks[c].is_set_n(b, n)
    }

    /// Are all `n` bits starting at `idx` clear?
    pub fn is_clear_n(&self, idx: usize, n: usize) -> bool {
        let (c, b) = self.chunk_of(idx);
        if b + n > CHUNK_BITS {
            return false;
        }
        self.chunks[c].is_clear_n(b, n)
    }

    /// Find a single set bit, clear it, and return its index.
    pub fn try_find_and_clear(&self, tseq: usize) -> Option<usize> {
        self.try_find_and_clear_n(1, tseq)
    }

    /// Find `n` contiguous set bits (within a single chunk for `n <= CHUNK_BITS`)
    /// and atomically clear them. `tseq` rotates the starting chunk to reduce
    /// contention between threads.
    pub fn try_find_and_clear_n(&self, n: usize, tseq: usize) -> Option<usize> {
        if n == 0 {
            return None;
        }
        let cc = self.chunk_count();
        if n <= CHUNK_BITS {
            let start = if cc == 0 { 0 } else { tseq % cc };
            for k in 0..cc {
                let c = (start + k) % cc;
                // Skip chunks the chunkmap marks empty (conservative).
                if !self.chunkmap.is_set(c) {
                    continue;
                }
                if let Some(b) = self.chunks[c].find_and_clear_n(n) {
                    self.chunkmap_clear_if_empty(c);
                    return Some(c * CHUNK_BITS + b);
                }
            }
            None
        } else {
            // Huge: a contiguous run spanning chunks. Rare; linear scan.
            self.try_find_and_clear_n_huge(n)
        }
    }

    /// Find a set bit whose `claim(idx)` returns true, clear it, and return its
    /// index (ports `mi_bitmap_try_find_and_claim`). The `claim` callback (a
    /// page-ownership CAS) is the serialization point; bits where it fails are
    /// left set. `tseq` rotates the starting chunk.
    pub fn try_find_and_claim(
        &self,
        tseq: usize,
        mut claim: impl FnMut(usize) -> bool,
    ) -> Option<usize> {
        let cc = self.chunk_count();
        if cc == 0 {
            return None;
        }
        let start = tseq % cc;
        for k in 0..cc {
            let c = (start + k) % cc;
            // Skip chunks the chunkmap marks empty (conservative).
            if !self.chunkmap.is_set(c) {
                continue;
            }
            for b in 0..CHUNK_BITS {
                if self.chunks[c].is_set(b) {
                    let idx = c * CHUNK_BITS + b;
                    if claim(idx) {
                        self.clear(idx);
                        return Some(idx);
                    }
                }
            }
        }
        None
    }

    /// Cross-chunk contiguous allocation for `n > CHUNK_BITS`.
    fn try_find_and_clear_n_huge(&self, n: usize) -> Option<usize> {
        let total = self.max_bits();
        let mut start = 0usize;
        while start + n <= total {
            if self.range_is_set(start, n) {
                if self.range_try_clear(start, n) {
                    return Some(start);
                }
                continue;
            }
            start += 1;
        }
        None
    }

    /// Whole-bitmap range predicate (may span chunks).
    fn range_is_set(&self, idx: usize, n: usize) -> bool {
        let mut remaining = n;
        let mut bit = idx;
        while remaining > 0 {
            let (c, b) = self.chunk_of(bit);
            let here = core::cmp::min(remaining, CHUNK_BITS - b);
            if !self.chunks[c].is_set_n(b, here) {
                return false;
            }
            bit += here;
            remaining -= here;
        }
        true
    }

    /// Whole-bitmap range clear with rollback (may span chunks).
    fn range_try_clear(&self, idx: usize, n: usize) -> bool {
        let mut done: usize = 0;
        let mut remaining = n;
        let mut bit = idx;
        let mut ok = true;
        while remaining > 0 {
            let (c, b) = self.chunk_of(bit);
            let here = core::cmp::min(remaining, CHUNK_BITS - b);
            if self.chunks[c].try_clear_n(b, here) {
                done += here;
            } else {
                ok = false;
                break;
            }
            bit += here;
            remaining -= here;
        }
        if !ok {
            // roll back [idx, idx+done)
            let mut rb = done;
            let mut rbit = idx;
            while rb > 0 {
                let (c, b) = self.chunk_of(rbit);
                let here = core::cmp::min(rb, CHUNK_BITS - b);
                self.chunks[c].set_n(b, here);
                rbit += here;
                rb -= here;
            }
            return false;
        }
        for off in 0..n.div_ceil(CHUNK_BITS) + 1 {
            let c = idx / CHUNK_BITS + off;
            if c < self.chunks.len() {
                self.chunkmap_clear_if_empty(c);
            }
        }
        true
    }

    /// Total set-bit count.
    pub fn popcount(&self) -> usize {
        self.chunks.iter().map(BChunk::popcount).sum()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::vec::Vec;

    fn make(chunks: usize) -> (BChunk, Vec<BChunk>) {
        let cm = BChunk::zeroed();
        let mut v = Vec::with_capacity(chunks);
        for _ in 0..chunks {
            v.push(BChunk::zeroed());
        }
        (cm, v)
    }

    #[test]
    fn single_bit_set_clear() {
        let (cm, chunks) = make(1);
        let bm = Bitmap::from_parts(&cm, &chunks);
        assert!(bm.set(5));
        assert!(bm.is_set(5));
        assert!(!bm.set(5)); // already set
        assert!(bm.clear(5));
        assert!(!bm.is_set(5));
        assert!(!bm.clear(5)); // already clear
    }

    #[test]
    fn find_and_clear_allocates_distinct() {
        let (cm, chunks) = make(2);
        let bm = Bitmap::from_parts(&cm, &chunks);
        // mark everything free
        // SAFETY: single-threaded test; `bm` is exclusively owned here.
        unsafe { bm.unsafe_set_n(0, bm.max_bits()) };
        assert_eq!(bm.popcount(), bm.max_bits());
        let a = bm.try_find_and_clear(0).unwrap();
        let b = bm.try_find_and_clear(0).unwrap();
        assert_ne!(a, b);
        assert!(!bm.is_set(a));
        assert!(!bm.is_set(b));
        assert_eq!(bm.popcount(), bm.max_bits() - 2);
    }

    #[test]
    fn contiguous_run_within_field() {
        let (cm, chunks) = make(1);
        let bm = Bitmap::from_parts(&cm, &chunks);
        // SAFETY: single-threaded test; `bm` is exclusively owned here.
        unsafe { bm.unsafe_set_n(0, CHUNK_BITS) };
        let idx = bm.try_find_and_clear_n(8, 0).unwrap();
        assert!(bm.is_clear_n(idx, 8));
        // freeing restores
        assert!(bm.set_n(idx, 8));
        assert!(bm.is_set_n(idx, 8));
    }

    #[test]
    fn contiguous_run_spanning_fields() {
        let (cm, chunks) = make(1);
        let bm = Bitmap::from_parts(&cm, &chunks);
        // SAFETY: single-threaded test; `bm` is exclusively owned here.
        unsafe { bm.unsafe_set_n(0, CHUNK_BITS) };
        // allocate one field, then a 64-bit run must straddle into the next field
        let _ = bm.try_find_and_clear_n(FIELD_BITS, 0).unwrap();
        let idx = bm.try_find_and_clear_n(FIELD_BITS, 0).unwrap();
        assert!(bm.is_clear_n(idx, FIELD_BITS));
    }

    #[test]
    fn full_then_empty() {
        let (cm, chunks) = make(1);
        let bm = Bitmap::from_parts(&cm, &chunks);
        // SAFETY: single-threaded test; `bm` is exclusively owned here.
        unsafe { bm.unsafe_set_n(0, CHUNK_BITS) };
        // allocate all single bits
        let mut seen = Vec::new();
        for _ in 0..CHUNK_BITS {
            seen.push(bm.try_find_and_clear(0).unwrap());
        }
        assert!(bm.try_find_and_clear(0).is_none()); // exhausted
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), CHUNK_BITS); // all distinct
        assert_eq!(bm.popcount(), 0);
        // chunkmap should now mark the chunk empty
        assert!(!bm.chunkmap.is_set(0));
    }

    #[test]
    fn huge_cross_chunk_run() {
        let (cm, chunks) = make(3);
        let bm = Bitmap::from_parts(&cm, &chunks);
        // SAFETY: single-threaded test; `bm` is exclusively owned here.
        unsafe { bm.unsafe_set_n(0, bm.max_bits()) };
        let n = CHUNK_BITS + 100; // spans two chunks
        let idx = bm.try_find_and_clear_n(n, 0).unwrap();
        assert!(!bm.range_is_set_for_test(idx, n));
    }

    impl Bitmap<'_> {
        fn range_is_set_for_test(&self, idx: usize, n: usize) -> bool {
            self.range_is_set(idx, n)
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    extern crate alloc;
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    /// Two threads concurrently allocating single bits from a small free bitmap
    /// must never get the same bit, and the total claimed must be exact.
    #[test]
    fn concurrent_find_and_clear_no_double_alloc() {
        loom::model(|| {
            // 4 free bits, 2 threads each take 2 — bounded state space.
            let cm = Arc::new(BChunk::zeroed());
            let chunk = Arc::new(BChunk::zeroed());
            // mark 4 bits free (non-atomic init, before sharing)
            chunk.set_n(0, 4);
            cm.set_n(0, 1);

            let mut handles = Vec::new();
            for _ in 0..2 {
                let cm = cm.clone();
                let chunk = chunk.clone();
                handles.push(loom::thread::spawn(move || {
                    let chunks = core::slice::from_ref(&*chunk);
                    let bm = Bitmap::from_parts(&cm, chunks);
                    let a = bm.try_find_and_clear(0);
                    let b = bm.try_find_and_clear(0);
                    (a, b)
                }));
            }
            let mut got = Vec::new();
            for h in handles {
                let (a, b) = h.join().unwrap();
                if let Some(x) = a {
                    got.push(x);
                }
                if let Some(x) = b {
                    got.push(x);
                }
            }
            // All 4 bits claimed, all distinct, none left.
            got.sort_unstable();
            let mut d = got.clone();
            d.dedup();
            assert_eq!(d.len(), got.len(), "double allocation: {got:?}");
            assert_eq!(got.len(), 4, "lost or extra allocations: {got:?}");
            assert_eq!(chunk.popcount(), 0);
        });
    }

    /// Arena purge claim protocol: a purger (`clear_n` → purge → `set_n`) and an
    /// allocator (`try_find_and_clear`) must never own the same slot at once.
    #[test]
    fn purge_claim_never_overlaps_alloc() {
        use crate::atomic::{AtomicUsize, Ordering};
        loom::model(|| {
            let cm = Arc::new(BChunk::zeroed());
            let chunk = Arc::new(BChunk::zeroed());
            chunk.set_n(0, 2); // bits 0,1 free
            cm.set_n(0, 1);
            let owner0 = Arc::new(AtomicUsize::new(0)); // 0=free, 1=purger, 2=alloc

            // Purger: claim slot 0, (purge), release.
            let (cm_p, chunk_p, own_p) = (cm.clone(), chunk.clone(), owner0.clone());
            let p = loom::thread::spawn(move || {
                let bm = Bitmap::from_parts(&cm_p, core::slice::from_ref(&*chunk_p));
                if bm.clear_n(0, 1) {
                    // Exclusively ours now — no allocator may hold it.
                    assert!(
                        own_p
                            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok(),
                        "purge claimed a slot an allocation owns"
                    );
                    own_p.store(0, Ordering::Release);
                    bm.set_n(0, 1); // release back to free
                }
            });

            // Allocator: grab a free slot; if it is slot 0, it owns it.
            let (cm_a, chunk_a, own_a) = (cm.clone(), chunk.clone(), owner0.clone());
            let a = loom::thread::spawn(move || {
                let bm = Bitmap::from_parts(&cm_a, core::slice::from_ref(&*chunk_a));
                if let Some(x) = bm.try_find_and_clear(0) {
                    if x == 0 {
                        assert!(
                            own_a
                                .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
                                .is_ok(),
                            "allocation took a slot a purge owns"
                        );
                    }
                }
            });
            p.join().unwrap();
            a.join().unwrap();
        });
    }

    /// Two concurrent purgers claiming + releasing the same slot must never both
    /// hold it at once (the `clear_n` CAS gate is the exclusion). Models the
    /// `delay==0`/`force` case where two `run_purge` calls overlap on one arena.
    #[test]
    fn two_purgers_never_overlap() {
        use crate::atomic::{AtomicUsize, Ordering};
        loom::model(|| {
            let cm = Arc::new(BChunk::zeroed());
            let chunk = Arc::new(BChunk::zeroed());
            chunk.set_n(0, 1); // one free slot
            cm.set_n(0, 1);
            let owner = Arc::new(AtomicUsize::new(0)); // 0 = unheld

            let mut handles = Vec::new();
            for id in 1..=2u32 {
                let (cm, chunk, owner) = (cm.clone(), chunk.clone(), owner.clone());
                handles.push(loom::thread::spawn(move || {
                    let bm = Bitmap::from_parts(&cm, core::slice::from_ref(&*chunk));
                    if bm.clear_n(0, 1) {
                        // Claimed: must be exclusive while held.
                        assert!(
                            owner
                                .compare_exchange(
                                    0,
                                    id as usize,
                                    Ordering::AcqRel,
                                    Ordering::Acquire
                                )
                                .is_ok(),
                            "two purgers held the same slot at once"
                        );
                        owner.store(0, Ordering::Release);
                        bm.set_n(0, 1); // release
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            assert!(chunk.is_set(0), "slot must be released back to free");
        });
    }
}
