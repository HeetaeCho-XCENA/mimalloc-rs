// SPDX-License-Identifier: MIT
//! C-ABI export layer (ports `mimalloc.h`'s public symbols).
//!
//! Enabled by the `capi` feature. Defines `#[no_mangle] extern "C"` `mi_*`
//! symbols with mimalloc-compatible signatures so the crate can be built as a
//! `cdylib`/`staticlib` and consumed from C/C++ (or used to validate behavior
//! against the C library). Off by default to avoid symbol clashes when used as
//! an ordinary Rust crate.
//!
//! All entry points route through the calling thread's default heap (so they
//! require `std`). Not yet ported: `mi_realpath`, `valloc`/`pvalloc`, the
//! `mi_heap_*` C surface (Phase G), and the malloc/`new` override shims.
//!
//! Build a C-linkable library with, e.g.:
//! `cargo rustc --release --features capi --crate-type cdylib`.

use core::ffi::{c_char, c_int, c_void};
use core::ptr::NonNull;

use crate::heap;
use crate::init;

#[inline]
fn out(p: Option<NonNull<u8>>) -> *mut c_void {
    p.map_or(core::ptr::null_mut(), |x| x.as_ptr() as *mut c_void)
}

#[inline]
fn checked_total(count: usize, size: usize) -> Option<usize> {
    count.checked_mul(size)
}

// ---------------------------------------------------------------------------
// Basic allocation
// ---------------------------------------------------------------------------

/// `mi_malloc`: allocate `size` bytes.
#[no_mangle]
pub extern "C" fn mi_malloc(size: usize) -> *mut c_void {
    out(init::malloc(size))
}

/// `mi_zalloc`: allocate `size` zeroed bytes.
#[no_mangle]
pub extern "C" fn mi_zalloc(size: usize) -> *mut c_void {
    out(init::zalloc(size))
}

/// `mi_calloc`: allocate `count * size` zeroed bytes (overflow ⇒ null).
#[no_mangle]
pub extern "C" fn mi_calloc(count: usize, size: usize) -> *mut c_void {
    match checked_total(count, size) {
        Some(total) => out(init::zalloc(total)),
        None => core::ptr::null_mut(),
    }
}

/// `mi_mallocn`: allocate `count * size` bytes (uninitialized; overflow ⇒ null).
#[no_mangle]
pub extern "C" fn mi_mallocn(count: usize, size: usize) -> *mut c_void {
    match checked_total(count, size) {
        Some(total) => out(init::malloc(total)),
        None => core::ptr::null_mut(),
    }
}

/// `mi_free`: free `p` (null is a no-op).
///
/// # Safety
/// `p` is null or a live allocation from this allocator.
#[no_mangle]
pub unsafe extern "C" fn mi_free(p: *mut c_void) {
    if let Some(nn) = NonNull::new(p as *mut u8) {
        // SAFETY: live allocation per contract.
        unsafe { init::free(nn) }
    }
}

/// `mi_usable_size`: usable bytes of `p` (0 if null).
///
/// # Safety
/// `p` is null or a live allocation from this allocator.
#[no_mangle]
pub unsafe extern "C" fn mi_usable_size(p: *mut c_void) -> usize {
    match NonNull::new(p as *mut u8) {
        None => 0,
        // SAFETY: live allocation per contract.
        Some(nn) => unsafe { heap::usable_size(nn) },
    }
}

/// `mi_realloc`: resize `p` to `newsize`.
///
/// # Safety
/// `p` is null or a live allocation; on success the old pointer is invalidated.
#[no_mangle]
pub unsafe extern "C" fn mi_realloc(p: *mut c_void, newsize: usize) -> *mut c_void {
    match NonNull::new(p as *mut u8) {
        None => out(init::malloc(newsize)),
        // SAFETY: live allocation per contract.
        Some(nn) => out(unsafe { init::realloc(nn, newsize) }),
    }
}

/// `mi_reallocn`: resize `p` to `count * size` (overflow ⇒ null).
///
/// # Safety
/// As [`mi_realloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_reallocn(p: *mut c_void, count: usize, size: usize) -> *mut c_void {
    match checked_total(count, size) {
        // SAFETY: forwarded contract.
        Some(total) => unsafe { mi_realloc(p, total) },
        None => core::ptr::null_mut(),
    }
}

/// `mi_recalloc`: resize `p` to `count * size`, zeroing any grown region.
///
/// # Safety
/// As [`mi_realloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_recalloc(p: *mut c_void, count: usize, size: usize) -> *mut c_void {
    let Some(total) = checked_total(count, size) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `p` is null or live.
    let old = unsafe { mi_usable_size(p) };
    // SAFETY: forwarded contract.
    let np = unsafe { mi_realloc(p, total) };
    if !np.is_null() && total > old {
        // SAFETY: `np` is valid for `total` bytes; zero the grown tail.
        unsafe { core::ptr::write_bytes((np as *mut u8).add(old), 0, total - old) };
    }
    np
}

/// `mi_expand`: grow `p` in place to `newsize` without moving, else null.
///
/// # Safety
/// `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_expand(p: *mut c_void, newsize: usize) -> *mut c_void {
    match NonNull::new(p as *mut u8) {
        None => core::ptr::null_mut(),
        // SAFETY: live allocation; fits-in-place check via usable size.
        Some(nn) => {
            if newsize <= unsafe { heap::usable_size(nn) } {
                p
            } else {
                core::ptr::null_mut()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Aligned allocation
// ---------------------------------------------------------------------------

/// Allocate `size` bytes so that `ptr + offset` is `align`-aligned.
fn aligned_at(size: usize, align: usize, offset: usize) -> Option<NonNull<u8>> {
    if !align.is_power_of_two() {
        return None;
    }
    if offset == 0 {
        return init::malloc_aligned(size, align);
    }
    // Over-allocate so an interior pointer `r` with `(r+offset) % align == 0`
    // fits in one block; `free` recovers the block start via the page-map.
    let p = init::malloc(size + align)?;
    let base = p.addr().get();
    let r = base + ((align - ((base + offset) & (align - 1))) & (align - 1));
    // SAFETY: `r` is within `[base, base+align)` and the block is ≥ size+align.
    Some(unsafe { NonNull::new_unchecked(p.as_ptr().with_addr(r)) })
}

/// `mi_malloc_aligned`.
#[no_mangle]
pub extern "C" fn mi_malloc_aligned(size: usize, alignment: usize) -> *mut c_void {
    out(aligned_at(size, alignment, 0))
}

/// `mi_malloc_aligned_at`.
#[no_mangle]
pub extern "C" fn mi_malloc_aligned_at(
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    out(aligned_at(size, alignment, offset))
}

/// `mi_zalloc_aligned`.
#[no_mangle]
pub extern "C" fn mi_zalloc_aligned(size: usize, alignment: usize) -> *mut c_void {
    let p = aligned_at(size, alignment, 0);
    if let Some(nn) = p {
        // SAFETY: valid for `size` bytes.
        unsafe { core::ptr::write_bytes(nn.as_ptr(), 0, size) };
    }
    out(p)
}

/// `mi_calloc_aligned` (overflow ⇒ null).
#[no_mangle]
pub extern "C" fn mi_calloc_aligned(count: usize, size: usize, alignment: usize) -> *mut c_void {
    match checked_total(count, size) {
        Some(total) => mi_zalloc_aligned(total, alignment),
        None => core::ptr::null_mut(),
    }
}

/// `mi_realloc_aligned`.
///
/// # Safety
/// As [`mi_realloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_realloc_aligned(
    p: *mut c_void,
    newsize: usize,
    alignment: usize,
) -> *mut c_void {
    match NonNull::new(p as *mut u8) {
        None => out(aligned_at(newsize, alignment, 0)),
        // SAFETY: live allocation per contract.
        Some(nn) => out(unsafe { init::realloc_aligned(nn, newsize, alignment) }),
    }
}

/// `mi_free_size`: free `p` (size hint ignored — recovered from the page-map).
///
/// # Safety
/// As [`mi_free`].
#[no_mangle]
pub unsafe extern "C" fn mi_free_size(p: *mut c_void, _size: usize) {
    // SAFETY: forwarded contract.
    unsafe { mi_free(p) }
}

/// `mi_free_aligned`: free `p` (size/align hints ignored).
///
/// # Safety
/// As [`mi_free`].
#[no_mangle]
pub unsafe extern "C" fn mi_free_aligned(p: *mut c_void, _size: usize, _alignment: usize) {
    // SAFETY: forwarded contract.
    unsafe { mi_free(p) }
}

// ---------------------------------------------------------------------------
// POSIX / libc style
// ---------------------------------------------------------------------------

const EINVAL: c_int = 22;
const ENOMEM: c_int = 12;

/// `mi_posix_memalign`.
///
/// # Safety
/// `pp` must be a valid, writable `*mut *mut c_void`.
#[no_mangle]
pub unsafe extern "C" fn mi_posix_memalign(
    pp: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> c_int {
    if pp.is_null() {
        return EINVAL;
    }
    // alignment must be a power of two and a multiple of sizeof(void*).
    if !alignment.is_power_of_two() || alignment % core::mem::size_of::<*mut c_void>() != 0 {
        return EINVAL;
    }
    let p = aligned_at(size, alignment, 0);
    match p {
        None => ENOMEM,
        Some(nn) => {
            // SAFETY: `pp` is a valid out-pointer per contract.
            unsafe { *pp = nn.as_ptr() as *mut c_void };
            0
        }
    }
}

/// `mi_aligned_alloc` (C11).
#[no_mangle]
pub extern "C" fn mi_aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    out(aligned_at(size, alignment, 0))
}

/// `mi_memalign`.
#[no_mangle]
pub extern "C" fn mi_memalign(alignment: usize, size: usize) -> *mut c_void {
    out(aligned_at(size, alignment, 0))
}

/// `mi_strdup`: duplicate a NUL-terminated C string.
///
/// # Safety
/// `s` is null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn mi_strdup(s: *const c_char) -> *mut c_char {
    if s.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `s` is a valid C string per contract.
    let len = unsafe { c_strlen(s) };
    let p = init::malloc(len + 1);
    if let Some(nn) = p {
        // SAFETY: dst valid for len+1; src valid for len+1 (incl NUL).
        unsafe { core::ptr::copy_nonoverlapping(s as *const u8, nn.as_ptr(), len + 1) };
        nn.as_ptr() as *mut c_char
    } else {
        core::ptr::null_mut()
    }
}

/// `mi_strndup`: duplicate at most `n` bytes of a C string (NUL-terminated).
///
/// # Safety
/// `s` is null or points to at least `min(strlen(s), n)` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn mi_strndup(s: *const c_char, n: usize) -> *mut c_char {
    if s.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `s` valid per contract.
    let len = unsafe { c_strnlen(s, n) };
    let p = init::malloc(len + 1);
    if let Some(nn) = p {
        // SAFETY: copy `len` bytes then NUL-terminate.
        unsafe {
            core::ptr::copy_nonoverlapping(s as *const u8, nn.as_ptr(), len);
            *nn.as_ptr().add(len) = 0;
        }
        nn.as_ptr() as *mut c_char
    } else {
        core::ptr::null_mut()
    }
}

/// # Safety
/// `s` is a valid NUL-terminated C string.
unsafe fn c_strlen(s: *const c_char) -> usize {
    let mut n = 0;
    // SAFETY: walk until NUL per contract.
    unsafe {
        while *s.add(n) != 0 {
            n += 1;
        }
    }
    n
}

/// # Safety
/// `s` points to at least `min(strlen(s), max)+1` readable bytes.
unsafe fn c_strnlen(s: *const c_char, max: usize) -> usize {
    let mut n = 0;
    // SAFETY: walk until NUL or `max` per contract.
    unsafe {
        while n < max && *s.add(n) != 0 {
            n += 1;
        }
    }
    n
}

// ---------------------------------------------------------------------------
// C++ new/delete support (for mimalloc-new-delete.h)
// ---------------------------------------------------------------------------

/// `mi_new`: like `mi_malloc` but aborts (instead of returning null) on OOM,
/// matching `operator new`'s throwing contract from a C ABI.
#[no_mangle]
pub extern "C" fn mi_new(size: usize) -> *mut c_void {
    match init::malloc(size) {
        Some(p) => p.as_ptr() as *mut c_void,
        None => oom_abort(),
    }
}

/// `mi_new_aligned`: aborting aligned new.
#[no_mangle]
pub extern "C" fn mi_new_aligned(size: usize, alignment: usize) -> *mut c_void {
    match aligned_at(size, alignment, 0) {
        Some(p) => p.as_ptr() as *mut c_void,
        None => oom_abort(),
    }
}

/// `mi_new_n`: aborting `count * size` new (overflow ⇒ abort).
#[no_mangle]
pub extern "C" fn mi_new_n(count: usize, size: usize) -> *mut c_void {
    match checked_total(count, size).and_then(init::malloc) {
        Some(p) => p.as_ptr() as *mut c_void,
        None => oom_abort(),
    }
}

/// `mi_new_nothrow`: non-aborting new (null on failure).
#[no_mangle]
pub extern "C" fn mi_new_nothrow(size: usize) -> *mut c_void {
    out(init::malloc(size))
}

/// `mi_new_aligned_nothrow`.
#[no_mangle]
pub extern "C" fn mi_new_aligned_nothrow(size: usize, alignment: usize) -> *mut c_void {
    out(aligned_at(size, alignment, 0))
}

#[cold]
#[inline(never)]
fn oom_abort() -> *mut c_void {
    <crate::prim::DefaultPrim as crate::prim::Prim>::out_stderr("mimalloc-rs: out of memory\n");
    std::process::abort()
}

// ---------------------------------------------------------------------------
// Info
// ---------------------------------------------------------------------------

/// `mi_version`: e.g. 30302 for v3.3.2.
#[no_mangle]
pub extern "C" fn mi_version() -> c_int {
    crate::bits::MI_MALLOC_VERSION as c_int
}

/// `mi_good_size`: the usable size a `mi_malloc(size)` would return.
#[no_mangle]
pub extern "C" fn mi_good_size(size: usize) -> usize {
    heap::good_size(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_api_basic_roundtrip() {
        // SAFETY: standard C-style usage.
        unsafe {
            let p = mi_malloc(100);
            assert!(!p.is_null());
            assert!(mi_usable_size(p) >= 100);
            core::ptr::write_bytes(p as *mut u8, 0xAB, 100);
            let p2 = mi_realloc(p, 4096);
            assert_eq!(*(p2 as *const u8), 0xAB);
            mi_free(p2);

            let z = mi_calloc(8, 16);
            assert!(!z.is_null());
            assert_eq!(*(z as *const u8), 0);
            mi_free(z);

            assert!(mi_mallocn(usize::MAX, 2).is_null()); // overflow guarded
        }
    }

    #[test]
    fn c_api_aligned_and_posix() {
        // SAFETY: standard usage.
        unsafe {
            let a = mi_malloc_aligned(40, 128);
            assert_eq!(a.addr() % 128, 0);
            mi_free(a);

            let at = mi_malloc_aligned_at(64, 64, 8);
            assert_eq!(((at as usize) + 8) % 64, 0);
            mi_free(at);

            let mut pp: *mut c_void = core::ptr::null_mut();
            assert_eq!(mi_posix_memalign(&mut pp, 256, 1000), 0);
            assert_eq!(pp.addr() % 256, 0);
            mi_free(pp);
            // invalid alignment (not power of two)
            assert_eq!(mi_posix_memalign(&mut pp, 24, 100), EINVAL);
        }
    }

    #[test]
    fn c_api_recalloc_zeroes_growth() {
        // SAFETY: standard usage.
        unsafe {
            let p = mi_malloc(16);
            core::ptr::write_bytes(p as *mut u8, 0xFF, 16);
            let p2 = mi_recalloc(p, 1, 4096) as *mut u8;
            // grown region is zeroed
            assert_eq!(*p2.add(2048), 0);
            mi_free(p2 as *mut c_void);
        }
    }

    #[test]
    fn c_api_strdup_and_version() {
        // SAFETY: valid C string literal.
        unsafe {
            let s = c"hello mimalloc";
            let d = mi_strdup(s.as_ptr());
            assert!(!d.is_null());
            assert_eq!(c_strlen(d), 14);
            mi_free(d as *mut c_void);
        }
        assert_eq!(mi_version(), 30302);
        assert!(mi_good_size(1) >= 1);
    }
}
