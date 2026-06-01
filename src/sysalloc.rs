// SPDX-License-Identifier: MIT
//! Fallback to the real system allocator for foreign pointers under `override`.
//!
//! When the standard libc symbols are interposed (Phase N + `LD_PRELOAD`), every
//! `free`/`realloc`/`malloc_usable_size` in the process routes through us —
//! including pointers the C runtime allocated *before* our interposition (the
//! dynamic linker, locale, TLS) or via paths we did not intercept. Such foreign
//! pointers are detected by [`crate::heap::is_in_heap_region`] and handed back to
//! the *real* libc allocator resolved here via `dlsym(RTLD_NEXT, ...)` (the next
//! definition in the search order — i.e. the original libc one our exported
//! symbol shadows).
//!
//! ## Bootstrap / reentrancy
//! `dlsym` may itself allocate. If that allocation routes to *our* `malloc`
//! (override on) it is fine: our `malloc` never calls `dlsym`, so there is no
//! recursion. Heap init is lazy and never calls back into `malloc` (the metadata
//! allocator mmaps directly), so a foreign `free` arriving during early process
//! init resolves `free` via `dlsym` safely.
#![cfg(all(feature = "override", feature = "std"))]
use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicUsize, Ordering};

// Lazily-resolved real libc symbols (RTLD_NEXT = the next definition in the
// search order, i.e. the original libc one that our exported symbol shadows).
fn resolve(name: &[u8]) -> usize {
    // SAFETY: name is a NUL-terminated byte string; dlsym returns null or a fn ptr.
    unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const c_char) as usize }
}

/// Resolve `$sym` (a NUL-terminated byte literal) once and cache it; each call
/// site gets its own `static`. Returns 0 if unresolved (handled by callers).
macro_rules! cached {
    ($sym:literal) => {{
        static CACHE: AtomicUsize = AtomicUsize::new(0);
        let mut p = CACHE.load(Ordering::Relaxed);
        if p == 0 {
            p = resolve($sym);
            CACHE.store(p, Ordering::Relaxed); // 0 stays 0 (unresolved) — handled by callers
        }
        p
    }};
}

/// Free a foreign pointer with the real system `free`. No-op if unresolved.
///
/// # Safety
/// `ptr` must be null or a live allocation owned by the *system* allocator (not
/// by this crate) — i.e. a pointer for which [`crate::heap::is_in_heap_region`]
/// is false.
pub unsafe fn free(ptr: *mut c_void) {
    let f = cached!(b"free\0");
    if f != 0 {
        // SAFETY: `f` is the real libc free; `ptr` is a foreign (system) allocation.
        let func: unsafe extern "C" fn(*mut c_void) = unsafe { core::mem::transmute(f) };
        unsafe { func(ptr) };
    }
}

/// Realloc a foreign pointer with the real system `realloc`. Returns null if unresolved.
///
/// # Safety
/// As [`free`]: `ptr` must be null or a live *system* allocation.
pub unsafe fn realloc(ptr: *mut c_void, newsize: usize) -> *mut c_void {
    let f = cached!(b"realloc\0");
    if f == 0 {
        return core::ptr::null_mut();
    }
    // SAFETY: real libc realloc on a foreign allocation.
    let func: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void =
        unsafe { core::mem::transmute(f) };
    unsafe { func(ptr, newsize) }
}

/// `malloc_usable_size` of a foreign pointer. 0 if unresolved.
///
/// # Safety
/// As [`free`]: `ptr` must be null or a live *system* allocation.
pub unsafe fn usable_size(ptr: *mut c_void) -> usize {
    let f = cached!(b"malloc_usable_size\0");
    if f == 0 {
        return 0;
    }
    // SAFETY: real libc malloc_usable_size on a foreign allocation.
    let func: unsafe extern "C" fn(*const c_void) -> usize = unsafe { core::mem::transmute(f) };
    unsafe { func(ptr) }
}
