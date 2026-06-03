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
//! C++ `operator new`/`operator delete` (Itanium-mangled `_Znwm`/`_ZdlPv`, …)
//! are also exported (bottom of this file), mirroring mimalloc's
//! `mimalloc-new-delete.h`: without them a preloaded library only intercepts
//! `malloc`/`free`, so a C++ program's `new`/`delete` fall through to libstdc++'s
//! operators — an extra call layer into `malloc`/`free`, measurably slower on
//! `new`-heavy workloads (alloc-test spent ~6% of cycles in libstdc++ before).

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

// ---------------------------------------------------------------------------
// C++ `operator new` / `operator delete` (Itanium ABI mangled names)
//
// Mirrors mimalloc's `mimalloc-new-delete.h` so a preloaded library intercepts
// C++ allocations *directly* instead of letting `new`/`delete` route through
// libstdc++'s operators (an extra call frame into `malloc`/`free`). Each shim
// forwards to a `capi::mi_*` entry that already does the work. In the ABI,
// `std::align_val_t` is a `size_t` and `const std::nothrow_t&` is an ignored
// pointer argument.
//
// `operator new` follows this crate's `mi_new` contract: it **aborts** on OOM
// (it cannot throw `std::bad_alloc` across the C ABI) — a documented divergence
// from the C++ standard's throwing `new`, unobservable unless allocation fails.
//
// `#[allow(non_snake_case)]`: the names are fixed by the C++ ABI mangling.
#[allow(non_snake_case)]
mod cxx {
    use core::ffi::c_void;

    // operator new(size_t) / operator new[](size_t) — throwing (abort on OOM).
    #[no_mangle]
    pub extern "C" fn _Znwm(size: usize) -> *mut c_void {
        crate::capi::mi_new(size)
    }
    #[no_mangle]
    pub extern "C" fn _Znam(size: usize) -> *mut c_void {
        crate::capi::mi_new(size)
    }

    // nothrow new — null on OOM (the `nothrow_t&` argument is ignored).
    #[no_mangle]
    pub extern "C" fn _ZnwmRKSt9nothrow_t(size: usize, _nt: *const c_void) -> *mut c_void {
        crate::capi::mi_new_nothrow(size)
    }
    #[no_mangle]
    pub extern "C" fn _ZnamRKSt9nothrow_t(size: usize, _nt: *const c_void) -> *mut c_void {
        crate::capi::mi_new_nothrow(size)
    }

    // aligned new (C++17) — `align_val_t` is a `size_t`.
    #[no_mangle]
    pub extern "C" fn _ZnwmSt11align_val_t(size: usize, align: usize) -> *mut c_void {
        crate::capi::mi_new_aligned(size, align)
    }
    #[no_mangle]
    pub extern "C" fn _ZnamSt11align_val_t(size: usize, align: usize) -> *mut c_void {
        crate::capi::mi_new_aligned(size, align)
    }

    // aligned nothrow new.
    #[no_mangle]
    pub extern "C" fn _ZnwmSt11align_val_tRKSt9nothrow_t(
        size: usize,
        align: usize,
        _nt: *const c_void,
    ) -> *mut c_void {
        crate::capi::mi_new_aligned_nothrow(size, align)
    }
    #[no_mangle]
    pub extern "C" fn _ZnamSt11align_val_tRKSt9nothrow_t(
        size: usize,
        align: usize,
        _nt: *const c_void,
    ) -> *mut c_void {
        crate::capi::mi_new_aligned_nothrow(size, align)
    }

    // operator delete(void*) / delete[](void*).
    //
    // # Safety
    // `p` is null or a live allocation from this allocator.
    #[no_mangle]
    pub unsafe extern "C" fn _ZdlPv(p: *mut c_void) {
        unsafe { crate::capi::mi_free(p) }
    }
    #[no_mangle]
    pub unsafe extern "C" fn _ZdaPv(p: *mut c_void) {
        unsafe { crate::capi::mi_free(p) }
    }

    // nothrow delete (the `nothrow_t&` argument is ignored).
    //
    // # Safety
    // As `_ZdlPv`.
    #[no_mangle]
    pub unsafe extern "C" fn _ZdlPvRKSt9nothrow_t(p: *mut c_void, _nt: *const c_void) {
        unsafe { crate::capi::mi_free(p) }
    }
    #[no_mangle]
    pub unsafe extern "C" fn _ZdaPvRKSt9nothrow_t(p: *mut c_void, _nt: *const c_void) {
        unsafe { crate::capi::mi_free(p) }
    }

    // sized delete (C++14).
    //
    // # Safety
    // As `_ZdlPv`; `size` is the type's size (only used as a hint).
    #[no_mangle]
    pub unsafe extern "C" fn _ZdlPvm(p: *mut c_void, size: usize) {
        unsafe { crate::capi::mi_free_size(p, size) }
    }
    #[no_mangle]
    pub unsafe extern "C" fn _ZdaPvm(p: *mut c_void, size: usize) {
        unsafe { crate::capi::mi_free_size(p, size) }
    }

    // aligned delete.
    //
    // # Safety
    // As `_ZdlPv`.
    #[no_mangle]
    pub unsafe extern "C" fn _ZdlPvSt11align_val_t(p: *mut c_void, align: usize) {
        unsafe { crate::capi::mi_free_aligned(p, 0, align) }
    }
    #[no_mangle]
    pub unsafe extern "C" fn _ZdaPvSt11align_val_t(p: *mut c_void, align: usize) {
        unsafe { crate::capi::mi_free_aligned(p, 0, align) }
    }

    // aligned nothrow delete.
    //
    // # Safety
    // As `_ZdlPv`.
    #[no_mangle]
    pub unsafe extern "C" fn _ZdlPvSt11align_val_tRKSt9nothrow_t(
        p: *mut c_void,
        align: usize,
        _nt: *const c_void,
    ) {
        unsafe { crate::capi::mi_free_aligned(p, 0, align) }
    }
    #[no_mangle]
    pub unsafe extern "C" fn _ZdaPvSt11align_val_tRKSt9nothrow_t(
        p: *mut c_void,
        align: usize,
        _nt: *const c_void,
    ) {
        unsafe { crate::capi::mi_free_aligned(p, 0, align) }
    }

    // sized aligned delete (C++17).
    //
    // # Safety
    // As `_ZdlPv`.
    #[no_mangle]
    pub unsafe extern "C" fn _ZdlPvmSt11align_val_t(p: *mut c_void, size: usize, align: usize) {
        unsafe { crate::capi::mi_free_size_aligned(p, size, align) }
    }
    #[no_mangle]
    pub unsafe extern "C" fn _ZdaPvmSt11align_val_t(p: *mut c_void, size: usize, align: usize) {
        unsafe { crate::capi::mi_free_size_aligned(p, size, align) }
    }
}
