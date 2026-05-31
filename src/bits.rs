// SPDX-License-Identifier: MIT
//! Platform constants, bit primitives, and the size-class (bin) mapping.
//!
//! Every constant here is re-derived directly from the v3.3.2 C headers
//! (`include/mimalloc/bits.h`, `include/mimalloc/types.h`, `include/mimalloc.h`)
//! for the default configuration. Values are given for a 64-bit target
//! (`MI_INTPTR_SIZE == 8`), which is what the Linux-first port targets.

#![allow(clippy::unreadable_literal)]

// ---------------------------------------------------------------------------
// Version  (include/mimalloc.h)
// ---------------------------------------------------------------------------

/// `MI_MALLOC_VERSION`: major + 2-digit minor + 2-digit patch (v3.3.2).
pub const MI_MALLOC_VERSION: u32 = 30302;
/// Human-readable version string.
pub const MI_MALLOC_VERSION_STRING: &str = "3.3.2";

// ---------------------------------------------------------------------------
// Pointer / word sizes  (bits.h)
// ---------------------------------------------------------------------------

/// `MI_INTPTR_SHIFT`: log2 of the pointer size. 64-bit ⇒ 3.
pub const MI_INTPTR_SHIFT: usize = {
    // We only support 32- and 64-bit pointers; 64-bit is the primary target.
    assert!(
        core::mem::size_of::<usize>() == 8 || core::mem::size_of::<usize>() == 4,
        "mimalloc-rs supports only 32- or 64-bit pointers"
    );
    core::mem::size_of::<usize>().trailing_zeros() as usize
};
/// `MI_INTPTR_SIZE`: bytes in a pointer (8 on 64-bit).
pub const MI_INTPTR_SIZE: usize = 1 << MI_INTPTR_SHIFT;
/// `MI_INTPTR_BITS`: bits in a pointer (64 on 64-bit).
pub const MI_INTPTR_BITS: usize = MI_INTPTR_SIZE * 8;

/// `MI_SIZE_SHIFT`: log2 of `size_t` size. 64-bit ⇒ 3.
pub const MI_SIZE_SHIFT: usize = MI_INTPTR_SHIFT;
/// `MI_SIZE_SIZE`: bytes in a `size_t` (8 on 64-bit).
pub const MI_SIZE_SIZE: usize = 1 << MI_SIZE_SHIFT;
/// `MI_SIZE_BITS`: bits in a `size_t` (64 on 64-bit).
pub const MI_SIZE_BITS: usize = MI_SIZE_SIZE * 8;

pub const MI_KIB: usize = 1024;
pub const MI_MIB: usize = MI_KIB * MI_KIB;
pub const MI_GIB: usize = MI_MIB * MI_KIB;

/// `MI_MAX_ALIGN_SIZE`: `sizeof(max_align_t)` — 16 bytes.
pub const MI_MAX_ALIGN_SIZE: usize = 16;

/// Maximum user-space virtual address bits (x64 ⇒ 47).
pub const MI_MAX_VABITS: usize = if MI_INTPTR_SIZE > 4 { 47 } else { 32 };

/// `MI_PAGE_MAP_FLAT`: a flat page-map is only used when `MI_MAX_VABITS <= 40`.
/// On x64 (47 bits) the **2-level** page-map is used.
pub const MI_PAGE_MAP_FLAT: bool = MI_MAX_VABITS <= 40 && MI_SECURE == 0;

// ---------------------------------------------------------------------------
// Build-mode toggles  (types.h)  — mapped onto cargo features
// ---------------------------------------------------------------------------

/// `MI_SECURE` hardening level (0..=5). The `secure` feature raises it to 3.
pub const MI_SECURE: u8 = if cfg!(feature = "secure") { 3 } else { 0 };
/// `MI_DEBUG` level. The `debug` feature raises it to 2.
pub const MI_DEBUG: u8 = if cfg!(feature = "debug") { 2 } else { 0 };
/// `MI_PADDING`: append a canary/delta padding struct to each block.
/// Enabled by `debug` or `secure`.
pub const MI_PADDING: bool = cfg!(any(feature = "debug", feature = "secure"));
/// `MI_ENCODE_FREELIST`: encode free-list `next` pointers with random keys.
/// On whenever padding is on, or always under `secure`.
pub const MI_ENCODE_FREELIST: bool = MI_PADDING || cfg!(feature = "secure");

// ---------------------------------------------------------------------------
// Arena / page geometry  (types.h)
// ---------------------------------------------------------------------------

/// `MI_ARENA_SLICE_SHIFT`: 13 + `MI_SIZE_SHIFT` ⇒ 16 (64 KiB slices).
pub const MI_ARENA_SLICE_SHIFT: usize = 13 + MI_SIZE_SHIFT;
/// `MI_ARENA_SLICE_SIZE`: 64 KiB.
pub const MI_ARENA_SLICE_SIZE: usize = 1 << MI_ARENA_SLICE_SHIFT;
/// `MI_ARENA_SLICE_ALIGN`: slices are aligned to their size.
pub const MI_ARENA_SLICE_ALIGN: usize = MI_ARENA_SLICE_SIZE;

/// `MI_BCHUNK_BITS_SHIFT`: 6 + `MI_SIZE_SHIFT` ⇒ 9 (512-bit bitmap chunks).
pub const MI_BCHUNK_BITS_SHIFT: usize = 6 + MI_SIZE_SHIFT;
/// `MI_BCHUNK_BITS`: 512 bits per bitmap chunk.
pub const MI_BCHUNK_BITS: usize = 1 << MI_BCHUNK_BITS_SHIFT;

pub const MI_ARENA_MIN_OBJ_SLICES: usize = 1;
pub const MI_ARENA_MAX_CHUNK_OBJ_SLICES: usize = MI_BCHUNK_BITS;
pub const MI_ARENA_MIN_OBJ_SIZE: usize = MI_ARENA_MIN_OBJ_SLICES * MI_ARENA_SLICE_SIZE;
/// `MI_ARENA_MAX_CHUNK_OBJ_SIZE`: 32 MiB (512 * 64 KiB).
pub const MI_ARENA_MAX_CHUNK_OBJ_SIZE: usize = MI_ARENA_MAX_CHUNK_OBJ_SLICES * MI_ARENA_SLICE_SIZE;

/// `MI_SMALL_PAGE_SIZE`: 64 KiB.
pub const MI_SMALL_PAGE_SIZE: usize = MI_ARENA_MIN_OBJ_SIZE;
/// `MI_MEDIUM_PAGE_SIZE`: 512 KiB.
pub const MI_MEDIUM_PAGE_SIZE: usize = 8 * MI_SMALL_PAGE_SIZE;
/// `MI_LARGE_PAGE_SIZE`: 4 MiB.
pub const MI_LARGE_PAGE_SIZE: usize = MI_SIZE_SIZE * MI_MEDIUM_PAGE_SIZE;

/// Never allocate more than `PTRDIFF_MAX`.
pub const MI_MAX_ALLOC_SIZE: usize = isize::MAX as usize;
/// Minimal on-demand commit unit for a page.
pub const MI_PAGE_MIN_COMMIT_SIZE: usize = MI_ARENA_SLICE_SIZE;

// ---------------------------------------------------------------------------
// Object size classes  (types.h, mimalloc.h)  — MI_ENABLE_LARGE_PAGES = 1
// ---------------------------------------------------------------------------

pub const MI_PAGE_ALIGN: usize = MI_ARENA_SLICE_ALIGN;
pub const MI_PAGE_MIN_START_BLOCK_ALIGN: usize = MI_MAX_ALIGN_SIZE;
pub const MI_PAGE_MAX_START_BLOCK_ALIGN2: usize = 4 * MI_KIB;
pub const MI_PAGE_OSPAGE_BLOCK_ALIGN2: usize = 4 * MI_KIB;
pub const MI_PAGE_MAX_OVERALLOC_ALIGN: usize = MI_ARENA_SLICE_SIZE;

/// `MI_SMALL_MAX_OBJ_SIZE`: `(64 KiB − 4 KiB) / 6` = **10240** (= 10 KiB).
pub const MI_SMALL_MAX_OBJ_SIZE: usize = (MI_SMALL_PAGE_SIZE - MI_PAGE_OSPAGE_BLOCK_ALIGN2) / 6;
/// `MI_MEDIUM_MAX_OBJ_SIZE`: `(512 KiB − 4 KiB) / 6` ≈ 84 KiB. (large pages on)
pub const MI_MEDIUM_MAX_OBJ_SIZE: usize = (MI_MEDIUM_PAGE_SIZE - MI_PAGE_OSPAGE_BLOCK_ALIGN2) / 6;
/// `MI_LARGE_MAX_OBJ_SIZE`: `4 MiB / 8` = 512 KiB (must be a power of two). (large pages on)
pub const MI_LARGE_MAX_OBJ_SIZE: usize = MI_LARGE_PAGE_SIZE / 8;
/// `MI_LARGE_MAX_OBJ_WSIZE`: 512 KiB / 8 = 65536 words.
pub const MI_LARGE_MAX_OBJ_WSIZE: usize = MI_LARGE_MAX_OBJ_SIZE / MI_SIZE_SIZE;

/// `MI_SMALL_WSIZE_MAX`: 128 words.
pub const MI_SMALL_WSIZE_MAX: usize = 128;
/// `MI_SMALL_SIZE_MAX`: 128 * 8 = 1024 bytes.
pub const MI_SMALL_SIZE_MAX: usize = MI_SMALL_WSIZE_MAX * MI_INTPTR_SIZE;

// ---------------------------------------------------------------------------
// Bins  (types.h)
// ---------------------------------------------------------------------------

/// `MI_BIN_HUGE`: bin index for objects too large for size classes.
pub const MI_BIN_HUGE: usize = 73;
/// `MI_BIN_FULL`: the queue of full pages.
pub const MI_BIN_FULL: usize = MI_BIN_HUGE + 1; // 74
/// `MI_BIN_COUNT`: total number of bins (including full queue).
pub const MI_BIN_COUNT: usize = MI_BIN_FULL + 1; // 75

/// Padding struct size (`mi_padding_t` = canary u32 + delta u32 = 8 bytes).
pub const MI_PADDING_SIZE: usize = if MI_PADDING { 8 } else { 0 };
/// Padding size rounded up to whole words.
pub const MI_PADDING_WSIZE: usize = MI_PADDING_SIZE.div_ceil(MI_INTPTR_SIZE);
/// `MI_PAGES_DIRECT`: size of the small-size fast-lookup array
/// (`MI_SMALL_WSIZE_MAX + MI_PADDING_WSIZE + 1`) ⇒ 129 (release) / 130 (padded).
pub const MI_PAGES_DIRECT: usize = MI_SMALL_WSIZE_MAX + MI_PADDING_WSIZE + 1;

// ---------------------------------------------------------------------------
// Page flags & special thread ids  (types.h)
// ---------------------------------------------------------------------------

pub const MI_PAGE_IN_FULL_QUEUE: usize = 0x01;
pub const MI_PAGE_HAS_INTERIOR_POINTERS: usize = 0x02;
pub const MI_PAGE_FLAG_MASK: usize = 0x03;

/// Abandoned page (not in any theap queue).
pub const MI_THREADID_ABANDONED: usize = 0;
/// Abandoned page that is also mapped into an arena's `pages_abandoned` lists.
pub const MI_THREADID_ABANDONED_MAPPED: usize = MI_PAGE_FLAG_MASK + 1; // 4

// ---------------------------------------------------------------------------
// Bit primitives  (bits.h)
// ---------------------------------------------------------------------------

/// Count leading zeros, returning `MI_SIZE_BITS` for `0` (matches C `mi_clz`).
#[inline]
pub const fn mi_clz(x: usize) -> usize {
    x.leading_zeros() as usize
}

/// Count trailing zeros, returning `MI_SIZE_BITS` for `0` (matches C `mi_ctz`).
#[inline]
pub const fn mi_ctz(x: usize) -> usize {
    x.trailing_zeros() as usize
}

/// Population count.
#[inline]
pub const fn mi_popcount(x: usize) -> usize {
    x.count_ones() as usize
}

/// Rotate left.
#[inline]
pub const fn mi_rotl(x: usize, r: usize) -> usize {
    x.rotate_left((r & (MI_SIZE_BITS - 1)) as u32)
}

/// Rotate right.
#[inline]
pub const fn mi_rotr(x: usize, r: usize) -> usize {
    x.rotate_right((r & (MI_SIZE_BITS - 1)) as u32)
}

/// Bit-scan forward: index of the least-significant set bit, or `None` if `x == 0`.
#[inline]
pub const fn mi_bsf(x: usize) -> Option<usize> {
    if x == 0 {
        None
    } else {
        Some(mi_ctz(x))
    }
}

/// Bit-scan reverse: index of the most-significant set bit, or `None` if `x == 0`.
#[inline]
pub const fn mi_bsr(x: usize) -> Option<usize> {
    if x == 0 {
        None
    } else {
        Some(MI_SIZE_BITS - 1 - mi_clz(x))
    }
}

// ---------------------------------------------------------------------------
// Size → word size and bin mapping  (page-queue.c `mi_bin`, internal.h)
// ---------------------------------------------------------------------------

/// `_mi_wsize_from_size`: bytes → machine words (rounded up).
#[inline]
pub const fn wsize_from_size(size: usize) -> usize {
    size.div_ceil(MI_INTPTR_SIZE)
}

/// `mi_bin`: map an allocation size to its size-class bin.
///
/// Returns a value in `1..=MI_BIN_HUGE`. Faithful port of the **`MI_ALIGN2W`**
/// branch (active when `MI_MAX_ALIGN_SIZE == 2 * MI_INTPTR_SIZE`, i.e. 16-byte
/// max-align on a 64-bit target). Sizes `0..=8` words get exact bins; larger
/// sizes are spaced exponentially in ~12.5% increments using the top 3 bits.
#[inline]
pub const fn bin(size: usize) -> usize {
    let mut wsize = wsize_from_size(size);
    // MI_ALIGN2W: round small sizes to double-word bins.
    if wsize <= 8 {
        return if wsize <= 1 { 1 } else { (wsize + 1) & !1 };
    }
    if wsize > MI_LARGE_MAX_OBJ_WSIZE {
        return MI_BIN_HUGE;
    }
    wsize -= 1;
    // highest set bit index (wsize != 0 here)
    let b = MI_SIZE_BITS - 1 - mi_clz(wsize);
    // top 3 bits select the bin; subtract 3 because the first 8 sizes are exact.
    ((b << 2) + ((wsize >> (b - 2)) & 0x03)) - 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_constants() {
        assert_eq!(MI_INTPTR_SIZE, core::mem::size_of::<usize>());
        assert_eq!(MI_ARENA_SLICE_SIZE, 64 * MI_KIB);
        assert_eq!(MI_SMALL_PAGE_SIZE, 64 * MI_KIB);
        assert_eq!(MI_MEDIUM_PAGE_SIZE, 512 * MI_KIB);
        assert_eq!(MI_LARGE_PAGE_SIZE, 4 * MI_MIB);
        assert_eq!(MI_BCHUNK_BITS, 512);
        assert_eq!(MI_SMALL_SIZE_MAX, 1024);
        assert_eq!(MI_BIN_HUGE, 73);
        assert_eq!(MI_BIN_COUNT, 75);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn size_class_constants_64bit() {
        // Errata note: the planning digest said 10880 — the correct value is 10240.
        assert_eq!(MI_SMALL_MAX_OBJ_SIZE, 10240);
        assert_eq!(MI_MEDIUM_MAX_OBJ_SIZE, (512 * MI_KIB - 4 * MI_KIB) / 6);
        assert_eq!(MI_LARGE_MAX_OBJ_SIZE, 512 * MI_KIB);
        assert_eq!(MI_LARGE_MAX_OBJ_WSIZE, 65536);
    }

    #[test]
    fn wsize_rounding() {
        assert_eq!(wsize_from_size(0), 0);
        assert_eq!(wsize_from_size(1), 1);
        assert_eq!(wsize_from_size(8), 1);
        assert_eq!(wsize_from_size(9), 2);
        assert_eq!(wsize_from_size(16), 2);
        assert_eq!(wsize_from_size(17), 3);
    }

    /// Anchor points hand-derived from the C `mi_bin` (MI_ALIGN2W branch).
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn bin_anchor_points() {
        assert_eq!(bin(0), 1);
        assert_eq!(bin(8), 1); // wsize 1
        assert_eq!(bin(16), 2); // wsize 2 -> (3)&~1 = 2
        assert_eq!(bin(24), 4); // wsize 3 -> (4)&~1 = 4
        assert_eq!(bin(32), 4); // wsize 4 -> (5)&~1 = 4
        assert_eq!(bin(40), 6); // wsize 5 -> (6)&~1 = 6
        assert_eq!(bin(48), 6); // wsize 6
        assert_eq!(bin(56), 8); // wsize 7 -> 8
        assert_eq!(bin(64), 8); // wsize 8
        assert_eq!(bin(72), 9); // wsize 9 general branch
        assert_eq!(bin(1024), 24); // wsize 128 -> bin 24
                                   // Largest non-huge object (512 KiB, wsize 65536) maps to bin 60 under
                                   // the large-pages config; MI_BIN_HUGE (73) is reserved for sizes beyond.
        assert_eq!(bin(MI_LARGE_MAX_OBJ_SIZE), 60);
        assert!(bin(MI_LARGE_MAX_OBJ_SIZE) < MI_BIN_HUGE);
        assert_eq!(bin(MI_LARGE_MAX_OBJ_SIZE + 1), MI_BIN_HUGE);
        assert_eq!(bin(16 * MI_MIB), MI_BIN_HUGE);
    }

    /// Structural invariants from `internals/01`: bins are non-decreasing in
    /// size and bounded by `MI_BIN_HUGE`.
    #[test]
    fn bin_monotonic_and_bounded() {
        let mut prev = 0usize;
        let mut size = 0usize;
        while size <= MI_LARGE_MAX_OBJ_SIZE {
            let b = bin(size);
            assert!(
                (1..=MI_BIN_HUGE).contains(&b),
                "bin({size}) = {b} out of range"
            );
            assert!(b >= prev, "bin not monotonic at size {size}: {b} < {prev}");
            prev = b;
            size += 8;
        }
        assert!(bin(MI_LARGE_MAX_OBJ_SIZE + 1) == MI_BIN_HUGE);
    }
}
