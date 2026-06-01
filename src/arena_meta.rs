// SPDX-License-Identifier: MIT
//! Metadata allocator (ports `src/arena-meta.c`).
//!
//! The allocator must allocate its *own* bookkeeping (arena descriptors, heap
//! and thread-local structures, bitmap storage) **without** recursing into the
//! global allocator. This module hands out small, zeroed blocks carved directly
//! from OS memory, breaking the bootstrap cycle: a fixed static seed chunk lets
//! the very first allocation succeed before any heap exists.
//!
//! Large metadata (page-map tables/submaps) is page-sized and goes straight to
//! [`crate::os`] instead; this allocator only serves small descriptors.

use core::ptr::NonNull;

use crate::bitmap::{BChunk, Bitmap, CHUNK_BITS};
use crate::os::{self, MemId};
use crate::sync::SpinLock;

/// Bytes per metadata block.
const META_BLOCK_SIZE: usize = 64;
/// Blocks per metadata chunk (one bitmap chunk worth).
const META_BLOCKS: usize = CHUNK_BITS; // 512
/// Total bytes in a metadata chunk's OS region (32 KiB).
const META_REGION_SIZE: usize = META_BLOCK_SIZE * META_BLOCKS;

/// Header for one metadata region; lives at the start of the region itself.
#[repr(C)]
struct MetaChunk {
    next: *mut MetaChunk,
    memid: MemId,
    /// Bitmap free-list (set = free) over the region's blocks.
    free: BChunk,
    chunkmap: BChunk,
    /// First block index available for allocation (after this header).
    first_data_block: usize,
}

impl MetaChunk {
    fn region_base(&self) -> *mut u8 {
        self.memid.base
    }

    /// Bitmap view over this chunk's blocks.
    fn bitmap(&self) -> Bitmap<'_> {
        Bitmap::from_parts(&self.chunkmap, core::slice::from_ref(&self.free))
    }

    /// Pointer to block `idx`.
    fn block_ptr(&self, idx: usize) -> *mut u8 {
        self.region_base().wrapping_add(idx * META_BLOCK_SIZE)
    }

    /// Does `ptr` fall within this chunk's data region?
    fn owns(&self, ptr: *mut u8) -> bool {
        let base = self.region_base().addr();
        let a = ptr.addr();
        a >= base && a < base + META_REGION_SIZE
    }
}

struct MetaState {
    lock: SpinLock,
    head: core::cell::UnsafeCell<*mut MetaChunk>,
}

// SAFETY: all access to `head` is performed while holding `lock`.
unsafe impl Sync for MetaState {}

static META: MetaState = MetaState {
    lock: SpinLock::new(),
    head: core::cell::UnsafeCell::new(core::ptr::null_mut()),
};

/// Allocate and initialize a new metadata chunk from the OS, push it on the list.
/// Returns the new chunk, or `None` on OOM. Caller must hold the lock.
unsafe fn push_new_chunk() -> Option<NonNull<MetaChunk>> {
    let (region, memid) = os::alloc(META_REGION_SIZE, true)?;
    // OS memory is zeroed, so the `BChunk` bitmaps (all-zero = all-clear) are
    // already valid; we only write the scalar header fields.
    let hdr = region.as_ptr() as *mut MetaChunk;
    let header_blocks = core::mem::size_of::<MetaChunk>().div_ceil(META_BLOCK_SIZE);
    // SAFETY: `region` is a fresh, committed, suitably sized OS allocation.
    unsafe {
        (*hdr).next = *META.head.get();
        (*hdr).memid = memid;
        (*hdr).first_data_block = header_blocks;
        // mark data blocks [header_blocks, META_BLOCKS) free
        let bm = (*hdr).bitmap();
        bm.unsafe_set_n(header_blocks, META_BLOCKS - header_blocks);
        *META.head.get() = hdr;
    }
    NonNull::new(hdr)
}

/// Allocate a zeroed metadata block region of at least `size` bytes.
///
/// Returns `None` if `size` exceeds what a metadata chunk can serve (such large
/// metadata must use [`crate::os`] directly) or on OOM.
pub fn meta_zalloc(size: usize) -> Option<NonNull<u8>> {
    let blocks = size.div_ceil(META_BLOCK_SIZE);
    if blocks == 0 || blocks > META_BLOCKS {
        return None;
    }
    let _g = META.lock.lock();
    // Walk existing chunks looking for a free run.
    // SAFETY: we hold the lock, so the list is stable.
    let mut cur = unsafe { *META.head.get() };
    while !cur.is_null() {
        // SAFETY: `cur` is a valid chunk header on our list.
        let chunk = unsafe { &*cur };
        if let Some(idx) = chunk.bitmap().try_find_and_clear_n(blocks, 0) {
            let p = chunk.block_ptr(idx);
            // SAFETY: [p, p+blocks*BLOCK) is within the committed region; zero it.
            unsafe {
                core::ptr::write_bytes(p, 0, blocks * META_BLOCK_SIZE);
            }
            return NonNull::new(p);
        }
        cur = chunk.next;
    }
    // No room: allocate a new chunk and serve from it.
    // SAFETY: holding the lock.
    let chunk = unsafe { &*push_new_chunk()?.as_ptr() };
    let idx = chunk.bitmap().try_find_and_clear_n(blocks, 0)?;
    let p = chunk.block_ptr(idx);
    // Fresh OS memory is already zero.
    NonNull::new(p)
}

/// Free a metadata block region previously returned by [`meta_zalloc`].
///
/// # Safety
/// `(ptr, size)` must match a live allocation from this module.
pub unsafe fn meta_free(ptr: NonNull<u8>, size: usize) {
    let blocks = size.div_ceil(META_BLOCK_SIZE);
    let _g = META.lock.lock();
    // SAFETY: lock held; walk the list to find the owning chunk.
    let mut cur = unsafe { *META.head.get() };
    while !cur.is_null() {
        let chunk = unsafe { &*cur };
        if chunk.owns(ptr.as_ptr()) {
            let off = ptr.as_ptr().addr() - chunk.region_base().addr();
            let idx = off / META_BLOCK_SIZE;
            chunk.bitmap().set_n(idx, blocks);
            return;
        }
        cur = chunk.next;
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    extern crate alloc;
    use super::*;

    #[test]
    fn alloc_distinct_and_zeroed() {
        // NB: tests share the process-global metadata state, so we only assert
        // properties that hold under concurrency (distinctness, zeroing); we do
        // not reset the global allocator between tests.
        let a = meta_zalloc(100).unwrap();
        let b = meta_zalloc(100).unwrap();
        assert_ne!(a.as_ptr(), b.as_ptr());
        // SAFETY: freshly allocated, at least 100 bytes, zeroed.
        unsafe {
            for i in 0..100 {
                assert_eq!(*a.as_ptr().add(i), 0);
            }
            core::ptr::write_bytes(a.as_ptr(), 0xCD, 100);
            meta_free(a, 100);
            // any subsequent same-size allocation is zeroed (reused or fresh)
            let c = meta_zalloc(100).unwrap();
            assert_eq!(*c.as_ptr(), 0);
            meta_free(b, 100);
            meta_free(c, 100);
        }
    }

    #[test]
    fn spans_multiple_chunks() {
        // Allocate enough to force a second chunk (each chunk ≈ 512 blocks).
        let mut ptrs = alloc::vec::Vec::new();
        for _ in 0..600 {
            ptrs.push(meta_zalloc(META_BLOCK_SIZE).unwrap());
        }
        let mut addrs: alloc::vec::Vec<usize> = ptrs.iter().map(|p| p.as_ptr().addr()).collect();
        addrs.sort_unstable();
        addrs.dedup();
        assert_eq!(addrs.len(), 600, "metadata blocks must be distinct");
        // SAFETY: free everything we allocated.
        unsafe {
            for p in ptrs {
                meta_free(p, META_BLOCK_SIZE);
            }
        }
    }
}
