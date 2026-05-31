// SPDX-License-Identifier: MIT
//! Alignment helpers and `Layout` adapters.
//!
//! Ports the small alignment utilities from `include/mimalloc/internal.h`
//! (`_mi_align_up`, `_mi_align_down`, `_mi_is_power_of_two`, ...) and adds
//! convenience adapters for [`core::alloc::Layout`].

use crate::bits::{MI_INTPTR_SIZE, MI_MAX_ALIGN_SIZE};

/// `_mi_is_power_of_two`: true for `0` and exact powers of two.
#[inline]
pub const fn is_power_of_two(x: usize) -> bool {
    (x & x.wrapping_sub(1)) == 0
}

/// `_mi_align_up`: round `sz` up to the nearest multiple of `alignment`.
///
/// `alignment` must be non-zero. Returns `sz` unchanged when already aligned.
#[inline]
pub const fn align_up(sz: usize, alignment: usize) -> usize {
    debug_assert!(alignment != 0);
    let mask = alignment - 1;
    if is_power_of_two(alignment) {
        (sz + mask) & !mask
    } else {
        ((sz + mask) / alignment) * alignment
    }
}

/// `_mi_align_down`: round `sz` down to the nearest multiple of `alignment`.
#[inline]
pub const fn align_down(sz: usize, alignment: usize) -> usize {
    debug_assert!(alignment != 0);
    let mask = alignment - 1;
    if is_power_of_two(alignment) {
        sz & !mask
    } else {
        (sz / alignment) * alignment
    }
}

/// True if `addr` is a multiple of `alignment` (a power of two).
#[inline]
pub const fn is_aligned(addr: usize, alignment: usize) -> bool {
    debug_assert!(is_power_of_two(alignment));
    (addr & (alignment - 1)) == 0
}

/// The size mimalloc will actually serve for a `(size, align)` request.
///
/// mimalloc guarantees natural alignment for power-of-two sizes; for an
/// explicit `align` larger than the size's natural guarantee, the effective
/// block size is `size` rounded up so the block start lands on `align`.
#[inline]
pub const fn good_size(size: usize, align: usize) -> usize {
    let align = if align < MI_INTPTR_SIZE {
        MI_INTPTR_SIZE
    } else {
        align
    };
    align_up(if size == 0 { MI_INTPTR_SIZE } else { size }, align)
}

/// Decompose a [`Layout`] into `(size, align)`, clamping the alignment to at
/// least the platform's `max_align_t` so small allocations get natural
/// alignment for free (mimalloc's default guarantee).
///
/// [`Layout`]: core::alloc::Layout
#[inline]
pub fn size_align(layout: core::alloc::Layout) -> (usize, usize) {
    let align = layout
        .align()
        .max(MI_MAX_ALIGN_SIZE.min(default_align_for(layout.size())));
    (layout.size(), align.max(1))
}

/// Natural alignment mimalloc provides for a given size without an explicit
/// alignment request (power-of-two sizes are naturally aligned up to a page).
#[inline]
const fn default_align_for(size: usize) -> usize {
    if size >= MI_MAX_ALIGN_SIZE {
        MI_MAX_ALIGN_SIZE
    } else if size == 0 {
        MI_INTPTR_SIZE
    } else {
        // round the size up to a power of two, capped at MI_MAX_ALIGN_SIZE
        let p = size.next_power_of_two();
        if p > MI_MAX_ALIGN_SIZE {
            MI_MAX_ALIGN_SIZE
        } else {
            p
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_of_two() {
        assert!(is_power_of_two(0));
        assert!(is_power_of_two(1));
        assert!(is_power_of_two(16));
        assert!(!is_power_of_two(24));
        assert!(!is_power_of_two(6));
    }

    #[test]
    fn alignment_rounding() {
        assert_eq!(align_up(0, 8), 0);
        assert_eq!(align_up(1, 8), 8);
        assert_eq!(align_up(8, 8), 8);
        assert_eq!(align_up(9, 16), 16);
        assert_eq!(align_down(15, 8), 8);
        assert_eq!(align_down(16, 8), 16);
        // non-power-of-two alignment
        assert_eq!(align_up(10, 6), 12);
        assert_eq!(align_down(10, 6), 6);
    }

    #[test]
    fn aligned_predicate() {
        assert!(is_aligned(0, 16));
        assert!(is_aligned(32, 16));
        assert!(!is_aligned(8, 16));
    }

    #[test]
    fn layout_size_align() {
        let l = core::alloc::Layout::from_size_align(24, 8).unwrap();
        let (s, a) = size_align(l);
        assert_eq!(s, 24);
        assert!(a >= 8);
    }
}
