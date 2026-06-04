// SPDX-License-Identifier: MIT
//! Free-list blocks and their (optionally encoded) `next` links (ports the
//! `mi_block_t` parts of `free.c` + `internal.h`). A free block stores its
//! successor in its first word: **plain** (a real `*mut Block`, provenance
//! preserved) or **encoded** under `secure`/`debug` (`MI_ENCODE_FREELIST`, an
//! integer transform using exposed provenance) to detect free-list corruption.

use core::cell::Cell;

#[cfg(any(feature = "secure", feature = "debug"))]
use crate::bits::{mi_rotl, mi_rotr};

/// Whether free-list links are encoded (mirrors `MI_ENCODE_FREELIST`).
pub const ENCODED: bool = cfg!(any(feature = "secure", feature = "debug"));

/// A free-list node, overlaid on the first word of a free block.
#[repr(C)]
pub struct Block {
    #[cfg(not(any(feature = "secure", feature = "debug")))]
    next: Cell<*mut Block>,
    #[cfg(any(feature = "secure", feature = "debug"))]
    next: Cell<usize>,
}

impl Block {
    /// Read the successor of `self` (decoding if necessary).
    ///
    /// # Safety
    /// `self` must point at a valid free block; `keys` must match the page.
    #[inline]
    pub unsafe fn next(&self, keys: [usize; 2]) -> *mut Block {
        #[cfg(not(any(feature = "secure", feature = "debug")))]
        {
            let _ = keys;
            self.next.get()
        }
        #[cfg(any(feature = "secure", feature = "debug"))]
        {
            decode(self.next.get(), keys)
        }
    }

    /// Set the successor of `self` (encoding if necessary).
    ///
    /// # Safety
    /// `self` must point at a valid free block; `keys` must match the page.
    #[inline]
    pub unsafe fn set_next(&self, next: *mut Block, keys: [usize; 2]) {
        #[cfg(not(any(feature = "secure", feature = "debug")))]
        {
            let _ = keys;
            self.next.set(next);
        }
        #[cfg(any(feature = "secure", feature = "debug"))]
        {
            self.next.set(encode(next, keys));
        }
    }
}

/// Encode a pointer into a free-list word (`0` ⇒ null/end-of-list).
#[cfg(any(feature = "secure", feature = "debug"))]
#[inline]
fn encode(p: *mut Block, keys: [usize; 2]) -> usize {
    if p.is_null() {
        return 0;
    }
    let a = p.expose_provenance();
    mi_rotl(a ^ keys[1], keys[0]).wrapping_add(keys[0])
}

/// Decode a free-list word back into a pointer (`0` ⇒ null).
#[cfg(any(feature = "secure", feature = "debug"))]
#[inline]
fn decode(x: usize, keys: [usize; 2]) -> *mut Block {
    if x == 0 {
        return core::ptr::null_mut();
    }
    let a = mi_rotr(x.wrapping_sub(keys[0]), keys[0]) ^ keys[1];
    core::ptr::with_exposed_provenance_mut(a)
}

#[cfg(all(test, not(any(feature = "secure", feature = "debug"))))]
mod tests {
    use super::*;

    #[test]
    fn plain_next_roundtrip() {
        let mut a = Block {
            next: Cell::new(core::ptr::null_mut()),
        };
        let mut b = Block {
            next: Cell::new(core::ptr::null_mut()),
        };
        let pa: *mut Block = &mut a;
        let pb: *mut Block = &mut b;
        // SAFETY: blocks are valid; plain mode ignores keys.
        unsafe {
            a.set_next(pb, [0, 0]);
            assert_eq!(a.next([0, 0]), pb);
            b.set_next(core::ptr::null_mut(), [0, 0]);
            assert!(b.next([0, 0]).is_null());
            let _ = pa;
        }
    }
}

#[cfg(all(test, any(feature = "secure", feature = "debug")))]
mod encoded_tests {
    use super::*;

    #[test]
    fn encoded_next_roundtrip() {
        let a = Block { next: Cell::new(0) };
        let mut b = Block { next: Cell::new(0) };
        let pb: *mut Block = &mut b;
        let keys = [0x9e37_79b9_7f4a_7c15usize, 0xc2b2_ae3d_27d4_eb4f];
        // SAFETY: blocks valid; consistent keys.
        unsafe {
            a.set_next(pb, keys);
            assert_eq!(a.next(keys), pb);
            assert_ne!(a.next.get(), pb.expose_provenance()); // actually encoded
            a.set_next(core::ptr::null_mut(), keys);
            assert!(a.next(keys).is_null());
        }
    }
}
