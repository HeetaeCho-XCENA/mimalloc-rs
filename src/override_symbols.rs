// SPDX-License-Identifier: MIT
//! Standard libc allocation symbols for transparent `LD_PRELOAD` override.
//!
//! Enabled by the `override` feature (which pulls in `capi`). Defines the plain
//! `#[no_mangle] extern "C"` symbols (`malloc`, `free`, `calloc`, …) that libc
//! and the system loader resolve, so building this crate as a `cdylib` and
//! `LD_PRELOAD`-ing it transparently replaces the system allocator. Each symbol
//! is a thin shim forwarding to the corresponding `crate::capi::mi_*` function,
//! which already handles overflow, alignment, and foreign-pointer fallback (the
//! Phase M arena-membership routing in `init::free`/`realloc`/`usable_size`).
//!
//! ## Why gated on the `override_export` cfg (not just `not(test)`)
//!
//! Defining `#[no_mangle] malloc` in any binary that links libstd → libc would
//! interpose that process's own `malloc`, corrupting it or causing
//! duplicate-symbol errors. `not(test)` alone is insufficient: it excludes only
//! *unit*-test binaries; *integration*-test binaries (`tests/*.rs`) link this
//! crate with `cfg(test) = false`, so the raw symbols would still compile in and
//! interpose the whole integration-test process. To make the guarantee real,
//! these definitions require an explicit `override_export` cfg that no cargo
//! `test`/`build`/`bench` sets — it is opt-in only for the deliberately built
//! preload library:
//!
//! ```sh
//! RUSTFLAGS="--cfg override_export" \
//!   cargo rustc --release --features override --crate-type cdylib
//! ```
//!
//! The forwarding logic itself is covered by the existing `capi` `mi_*` tests.
//!
//! C++ `operator new`/`operator delete` (mangled `_Znwm`/`_ZdlPv`, …) are a
//! documented follow-up and intentionally not exported here.

use core::ffi::{c_char, c_int, c_void};

/// `malloc`: allocate `size` bytes.
#[no_mangle]
pub extern "C" fn malloc(size: usize) -> *mut c_void {
    crate::capi::mi_malloc(size)
}

/// `free`: free `p` (null is a no-op).
///
/// # Safety
/// `p` is null or a live allocation (ours or the system allocator's; foreign
/// pointers are routed back to the system allocator by `init::free`).
#[no_mangle]
pub unsafe extern "C" fn free(p: *mut c_void) {
    // SAFETY: forwarded contract.
    unsafe { crate::capi::mi_free(p) }
}

/// `calloc`: allocate `count * size` zeroed bytes (overflow ⇒ null).
#[no_mangle]
pub extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    crate::capi::mi_calloc(count, size)
}

/// `realloc`: resize `p` to `newsize`.
///
/// # Safety
/// `p` is null or a live allocation; on success the old pointer is invalidated.
#[no_mangle]
pub unsafe extern "C" fn realloc(p: *mut c_void, newsize: usize) -> *mut c_void {
    // SAFETY: forwarded contract.
    unsafe { crate::capi::mi_realloc(p, newsize) }
}

/// `aligned_alloc` (C11): allocate `size` bytes aligned to `alignment`.
#[no_mangle]
pub extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    crate::capi::mi_aligned_alloc(alignment, size)
}

/// `posix_memalign`: allocate `size` bytes aligned to `alignment`, storing the
/// result in `*memptr`. Returns 0 on success or an errno code.
///
/// # Safety
/// `memptr` must be a valid, writable `*mut *mut c_void`.
#[no_mangle]
pub unsafe extern "C" fn posix_memalign(
    memptr: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> c_int {
    // SAFETY: forwarded contract (`memptr` is a valid out-pointer).
    unsafe { crate::capi::mi_posix_memalign(memptr, alignment, size) }
}

/// `memalign`: allocate `size` bytes aligned to `alignment`.
#[no_mangle]
pub extern "C" fn memalign(alignment: usize, size: usize) -> *mut c_void {
    crate::capi::mi_memalign(alignment, size)
}

/// `valloc`: allocate `size` bytes aligned to the OS page size.
#[no_mangle]
pub extern "C" fn valloc(size: usize) -> *mut c_void {
    crate::capi::mi_valloc(size)
}

/// `pvalloc`: like [`valloc`] but `size` is rounded up to a whole number of OS
/// pages (overflow ⇒ null).
#[no_mangle]
pub extern "C" fn pvalloc(size: usize) -> *mut c_void {
    crate::capi::mi_pvalloc(size)
}

/// `reallocarray` (BSD): resize `p` to `count * size` (overflow ⇒ null + errno).
///
/// # Safety
/// As [`realloc`].
#[no_mangle]
pub unsafe extern "C" fn reallocarray(p: *mut c_void, count: usize, size: usize) -> *mut c_void {
    // SAFETY: forwarded contract.
    unsafe { crate::capi::mi_reallocarray(p, count, size) }
}

/// `reallocf` (BSD): resize `p` to `newsize`; on failure the original `p` is
/// freed before returning null.
///
/// # Safety
/// As [`realloc`].
#[no_mangle]
pub unsafe extern "C" fn reallocf(p: *mut c_void, newsize: usize) -> *mut c_void {
    // SAFETY: forwarded contract.
    let np = unsafe { crate::capi::mi_realloc(p, newsize) };
    if np.is_null() && !p.is_null() {
        // SAFETY: realloc failed and did not free `p`; free the original.
        unsafe { crate::capi::mi_free(p) };
    }
    np
}

/// `strdup`: duplicate a NUL-terminated C string.
///
/// # Safety
/// `s` is null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn strdup(s: *const c_char) -> *mut c_char {
    // SAFETY: forwarded contract.
    unsafe { crate::capi::mi_strdup(s) }
}

/// `strndup`: duplicate at most `n` bytes of a C string (NUL-terminated).
///
/// # Safety
/// `s` is null or points to at least `min(strlen(s), n)` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn strndup(s: *const c_char, n: usize) -> *mut c_char {
    // SAFETY: forwarded contract.
    unsafe { crate::capi::mi_strndup(s, n) }
}

/// `malloc_usable_size`: usable bytes of `p` (0 if null).
///
/// # Safety
/// `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn malloc_usable_size(p: *mut c_void) -> usize {
    // SAFETY: forwarded contract.
    unsafe { crate::capi::mi_malloc_usable_size(p as *const c_void) }
}

/// `cfree`: "checked free" — identical to [`free`] (null is a no-op).
///
/// # Safety
/// As [`free`].
#[no_mangle]
pub unsafe extern "C" fn cfree(p: *mut c_void) {
    // SAFETY: forwarded contract.
    unsafe { crate::capi::mi_cfree(p) }
}

/// `malloc_size` (macOS-ism, also exported by some C libs): usable bytes of `p`
/// (0 if null).
///
/// # Safety
/// `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn malloc_size(p: *const c_void) -> usize {
    // SAFETY: forwarded contract.
    unsafe { crate::capi::mi_malloc_size(p) }
}

/// `malloc_good_size`: the usable size a `malloc(size)` would return.
#[no_mangle]
pub extern "C" fn malloc_good_size(size: usize) -> usize {
    crate::capi::mi_malloc_good_size(size)
}
