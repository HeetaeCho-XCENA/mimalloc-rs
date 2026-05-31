// SPDX-License-Identifier: MIT
//! `GlobalAlloc` implementation — the drop-in global allocator
//! (ports the `mimalloc-new-delete` / override role).
//!
//! ```ignore
//! use mimalloc_rs::MiMalloc;
//! #[global_allocator]
//! static GLOBAL: MiMalloc = MiMalloc;
//! ```

/// The mimalloc-rs allocator handle. Zero-sized; all state is process-global.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct MiMalloc;

#[cfg(feature = "std")]
mod imp {
    use super::MiMalloc;
    use crate::init;
    use core::alloc::{GlobalAlloc, Layout};
    use core::ptr::NonNull;

    // SAFETY: `malloc_aligned` returns blocks honoring the requested size and
    // alignment (or null), `free` returns them to the owning page; these uphold
    // the `GlobalAlloc` contract.
    unsafe impl GlobalAlloc for MiMalloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            init::malloc_aligned(layout.size().max(1), layout.align())
                .map_or(core::ptr::null_mut(), |p| p.as_ptr())
        }

        unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
            if let Some(p) = NonNull::new(ptr) {
                // SAFETY: `ptr` came from this allocator per the contract.
                unsafe { init::free(p) }
            }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: forwarding to `alloc`.
            let p = unsafe { self.alloc(layout) };
            if !p.is_null() {
                // SAFETY: `p` is valid for `layout.size()` bytes.
                unsafe { core::ptr::write_bytes(p, 0, layout.size()) }
            }
            p
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            match NonNull::new(ptr) {
                None => {
                    // SAFETY: building a layout with the same align.
                    let l = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
                    // SAFETY: fresh alloc.
                    unsafe { self.alloc(l) }
                }
                // SAFETY: `ptr` is a live allocation with this layout's align.
                Some(p) => unsafe {
                    init::realloc_aligned(p, new_size, layout.align())
                        .map_or(core::ptr::null_mut(), |q| q.as_ptr())
                },
            }
        }
    }
}
