// SPDX-License-Identifier: MIT
//! `Allocator` trait implementations for [`MiMalloc`].
//!
//! The stable [`allocator_api2::alloc::Allocator`] impl is always available;
//! the unstable `core::alloc::Allocator` impl is added behind the `nightly`
//! feature so `Box::new_in`/`Vec::with_capacity_in` work on both channels.

#[cfg(feature = "std")]
mod stable {
    use crate::api::MiMalloc;
    use crate::init;
    use allocator_api2::alloc::{AllocError, Allocator};
    use core::alloc::Layout;
    use core::ptr::NonNull;

    // SAFETY: `allocate` returns blocks matching the layout's size/align (or
    // errs), `deallocate` returns them; `MiMalloc` is stateless so cloning it
    // yields an equivalent allocator, as the trait requires.
    unsafe impl Allocator for MiMalloc {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            let size = layout.size();
            let p = init::malloc_aligned(size.max(1), layout.align()).ok_or(AllocError)?;
            Ok(NonNull::slice_from_raw_parts(p, size))
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, _layout: Layout) {
            // SAFETY: `ptr` was produced by `allocate`.
            unsafe { init::free(ptr) }
        }
    }
}

#[cfg(all(feature = "std", feature = "nightly"))]
mod nightly {
    use crate::api::MiMalloc;
    use crate::init;
    use core::alloc::{AllocError, Allocator, Layout};
    use core::ptr::NonNull;

    // SAFETY: same invariants as the stable impl above.
    unsafe impl Allocator for MiMalloc {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            let size = layout.size();
            let p = init::malloc_aligned(size.max(1), layout.align()).ok_or(AllocError)?;
            Ok(NonNull::slice_from_raw_parts(p, size))
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, _layout: Layout) {
            // SAFETY: `ptr` was produced by `allocate`.
            unsafe { init::free(ptr) }
        }
    }
}
