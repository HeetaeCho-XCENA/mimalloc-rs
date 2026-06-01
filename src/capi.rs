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
//! require `std`). Not yet ported: `mi_realpath` and the malloc/`new` override
//! shims.
//!
//! Build a C-linkable library with, e.g.:
//! `cargo rustc --release --features capi --crate-type cdylib`.

use core::ffi::{c_char, c_int, c_long, c_void};
use core::ptr::NonNull;

use crate::heap::{self, Heap};
use crate::init;
use crate::options::{self, Opt};
use crate::stats;

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

/// `mi_malloc_usable_size`: usable bytes of `p` (0 if null) — `const void*`
/// alias of [`mi_usable_size`].
///
/// # Safety
/// `p` is null or a live allocation from this allocator.
#[no_mangle]
pub unsafe extern "C" fn mi_malloc_usable_size(p: *const c_void) -> usize {
    // SAFETY: forwarded contract (same logic as `mi_usable_size`).
    unsafe { mi_usable_size(p as *mut c_void) }
}

/// `mi_malloc_size`: macOS-style alias of [`mi_malloc_usable_size`].
///
/// # Safety
/// As [`mi_malloc_usable_size`].
#[no_mangle]
pub unsafe extern "C" fn mi_malloc_size(p: *const c_void) -> usize {
    // SAFETY: forwarded contract.
    unsafe { mi_malloc_usable_size(p) }
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
    // Guard against `size + align` overflow (return null rather than wrap).
    let p = init::malloc(size.checked_add(align)?)?;
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

/// `mi_aligned_recalloc`: resize `p` to `newcount * size` (overflow ⇒ null),
/// keeping the result `alignment`-aligned and zeroing any grown tail.
///
/// # Safety
/// As [`mi_realloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_aligned_recalloc(
    p: *mut c_void,
    newcount: usize,
    size: usize,
    alignment: usize,
) -> *mut c_void {
    let Some(total) = checked_total(newcount, size) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `p` is null or live.
    let old = unsafe { mi_usable_size(p) };
    // SAFETY: forwarded contract.
    let np = unsafe { mi_realloc_aligned(p, total, alignment) };
    if !np.is_null() && total > old {
        // SAFETY: `np` is valid for `total` bytes; zero the grown tail.
        unsafe { core::ptr::write_bytes((np as *mut u8).add(old), 0, total - old) };
    }
    np
}

/// `mi_recalloc_aligned`: identical to [`mi_aligned_recalloc`] (different name).
///
/// # Safety
/// As [`mi_realloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_recalloc_aligned(
    p: *mut c_void,
    newcount: usize,
    size: usize,
    alignment: usize,
) -> *mut c_void {
    // SAFETY: forwarded contract.
    unsafe { mi_aligned_recalloc(p, newcount, size, alignment) }
}

/// `mi_recalloc_aligned_at`: like [`mi_recalloc_aligned`] but the result is
/// `alignment`-aligned at `offset`, with any grown tail zeroed.
///
/// # Safety
/// As [`mi_realloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_recalloc_aligned_at(
    p: *mut c_void,
    newcount: usize,
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    let Some(total) = checked_total(newcount, size) else {
        return core::ptr::null_mut();
    };
    let Some(nn) = NonNull::new(p as *mut u8) else {
        // Fresh allocation: zeroed, aligned-at.
        let fresh = aligned_at(total, alignment, offset);
        if let Some(f) = fresh {
            // SAFETY: valid for `total` bytes.
            unsafe { core::ptr::write_bytes(f.as_ptr(), 0, total) };
        }
        return out(fresh);
    };
    // Resize path: `init::realloc_aligned` can't honor `offset`, so allocate a
    // fresh aligned-at block, copy, free the old block, then zero the tail.
    // SAFETY: live allocation per contract.
    let old = unsafe { heap::usable_size(nn) };
    let Some(np) = aligned_at(total, alignment, offset) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `np`/`nn` are valid for `min(old, total)` bytes and disjoint.
    unsafe {
        core::ptr::copy_nonoverlapping(nn.as_ptr(), np.as_ptr(), old.min(total));
        init::free(nn);
    }
    if total > old {
        // SAFETY: `np` is valid for `total` bytes; zero the grown tail.
        unsafe { core::ptr::write_bytes(np.as_ptr().add(old), 0, total - old) };
    }
    out(Some(np))
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

/// `mi_free_size_aligned`: free `p` (size/align hints ignored).
///
/// # Safety
/// As [`mi_free`].
#[no_mangle]
pub unsafe extern "C" fn mi_free_size_aligned(p: *mut c_void, _size: usize, _alignment: usize) {
    // SAFETY: forwarded contract.
    unsafe { mi_free(p) }
}

// ---------------------------------------------------------------------------
// POSIX / libc style
// ---------------------------------------------------------------------------

const EINVAL: c_int = 22;
const ENOMEM: c_int = 12;
/// `EOVERFLOW` errno value (Linux).
const EOVERFLOW: c_int = 75;

/// Set the calling thread's `errno`.
#[inline]
fn set_errno(code: c_int) {
    // SAFETY: `__errno_location` returns a valid per-thread errno slot on Linux.
    unsafe { *libc::__errno_location() = code };
}

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

/// `mi_cfree`: "checked free" — identical to [`mi_free`] (null is a no-op).
///
/// The C library checks that `p` lies in mimalloc's heap before freeing; our
/// single-allocator model recovers every block via the page-map in `free`, so a
/// plain forward is correct.
///
/// # Safety
/// As [`mi_free`].
#[no_mangle]
pub unsafe extern "C" fn mi_cfree(p: *mut c_void) {
    // SAFETY: forwarded contract.
    unsafe { mi_free(p) }
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
// libc / BSD compatibility
// ---------------------------------------------------------------------------

/// `mi_valloc`: allocate `size` bytes aligned to the OS page size.
#[no_mangle]
pub extern "C" fn mi_valloc(size: usize) -> *mut c_void {
    mi_memalign(crate::os::page_size(), size)
}

/// `mi_pvalloc`: like [`mi_valloc`] but `size` is first rounded up to a whole
/// number of OS pages (overflow ⇒ null).
#[no_mangle]
pub extern "C" fn mi_pvalloc(size: usize) -> *mut c_void {
    let psize = crate::os::page_size();
    if size >= usize::MAX - psize {
        return core::ptr::null_mut();
    }
    // `psize` is a power of two, so round `size` up to a multiple of it.
    let asize = (size + psize - 1) & !(psize - 1);
    mi_memalign(psize, asize)
}

/// `mi_reallocarray` (BSD): resize `p` to `count * size`. On overflow sets
/// `errno = EOVERFLOW` and returns null; on OOM sets `errno = ENOMEM`.
///
/// # Safety
/// As [`mi_realloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_reallocarray(p: *mut c_void, count: usize, size: usize) -> *mut c_void {
    let Some(total) = checked_total(count, size) else {
        set_errno(EOVERFLOW);
        return core::ptr::null_mut();
    };
    // SAFETY: forwarded contract.
    let np = unsafe { mi_realloc(p, total) };
    if np.is_null() {
        set_errno(ENOMEM);
    }
    np
}

/// `mi_reallocarr` (NetBSD): resize `*ptrp` to `count * size`, writing the new
/// pointer back through `ptrp`. Returns 0 on success or the failing errno code.
///
/// # Safety
/// `ptrp` is null or a valid, writable `*mut *mut c_void`; `*ptrp` is null or a
/// live allocation from this allocator.
#[no_mangle]
pub unsafe extern "C" fn mi_reallocarr(ptrp: *mut c_void, count: usize, size: usize) -> c_int {
    if ptrp.is_null() || size == 0 {
        set_errno(EINVAL);
        return EINVAL;
    }
    let Some(total) = checked_total(count, size) else {
        set_errno(EOVERFLOW);
        return EOVERFLOW;
    };
    let op = ptrp as *mut *mut c_void;
    if total == 0 {
        // SAFETY: `op` is a valid `*mut *mut c_void`; `*op` is null or live.
        unsafe {
            mi_free(*op);
            *op = core::ptr::null_mut();
        }
        0
    } else {
        // SAFETY: `*op` is null or a live allocation per contract.
        let newp = unsafe { mi_realloc(*op, total) };
        if newp.is_null() {
            set_errno(ENOMEM);
            return ENOMEM;
        }
        // SAFETY: `op` is a valid out-pointer per contract.
        unsafe { *op = newp };
        0
    }
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

/// `mi_malloc_good_size`: macOS-style alias of [`mi_good_size`].
#[no_mangle]
pub extern "C" fn mi_malloc_good_size(size: usize) -> usize {
    heap::good_size(size)
}

// ---------------------------------------------------------------------------
// First-class heaps (`mi_heap_t*` == `*mut Heap`)
//
// Contract (matching mimalloc): a heap is owned by the thread that created it
// via `mi_heap_new`; `mi_heap_*` allocation calls must be made only from that
// thread (or under external synchronization). Blocks may be freed from any
// thread (`mi_free` is heap-independent and routes owner-vs-cross-thread).
// ---------------------------------------------------------------------------

/// `mi_heap_new`: create a first-class heap (null on OOM).
#[no_mangle]
pub extern "C" fn mi_heap_new() -> *mut Heap {
    Heap::new_boxed(init::process_keys(), init::current_tid())
        .map_or(core::ptr::null_mut(), |h| h.as_ptr())
}

/// `mi_heap_delete`: free the heap; outstanding blocks stay valid (abandoned).
///
/// # Safety
/// `heap` is a live heap from [`mi_heap_new`], not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_delete(heap: *mut Heap) {
    if let Some(h) = NonNull::new(heap) {
        // SAFETY: live heap per contract.
        unsafe { Heap::delete(h) }
    }
}

/// `mi_heap_destroy`: free the heap and **all** its blocks at once.
///
/// # Safety
/// `heap` is live; no block of it may be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_destroy(heap: *mut Heap) {
    if let Some(h) = NonNull::new(heap) {
        // SAFETY: live heap; caller guarantees no outstanding use.
        unsafe { Heap::destroy(h) }
    }
}

/// `mi_heap_malloc`.
///
/// # Safety
/// `heap` is a live heap from [`mi_heap_new`].
#[no_mangle]
pub unsafe extern "C" fn mi_heap_malloc(heap: *mut Heap, size: usize) -> *mut c_void {
    match NonNull::new(heap) {
        None => core::ptr::null_mut(),
        // SAFETY: live heap per contract.
        Some(h) => out(unsafe { h.as_ref() }.alloc(size)),
    }
}

/// `mi_heap_zalloc`.
///
/// # Safety
/// As [`mi_heap_malloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_heap_zalloc(heap: *mut Heap, size: usize) -> *mut c_void {
    // SAFETY: forwarded contract.
    let p = unsafe { mi_heap_malloc(heap, size) };
    if !p.is_null() {
        // SAFETY: valid for `size` bytes.
        unsafe { core::ptr::write_bytes(p as *mut u8, 0, size) };
    }
    p
}

/// `mi_heap_calloc` (overflow ⇒ null).
///
/// # Safety
/// As [`mi_heap_malloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_heap_calloc(heap: *mut Heap, count: usize, size: usize) -> *mut c_void {
    match checked_total(count, size) {
        // SAFETY: forwarded contract.
        Some(total) => unsafe { mi_heap_zalloc(heap, total) },
        None => core::ptr::null_mut(),
    }
}

/// `mi_heap_malloc_aligned`.
///
/// # Safety
/// As [`mi_heap_malloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_heap_malloc_aligned(
    heap: *mut Heap,
    size: usize,
    alignment: usize,
) -> *mut c_void {
    match NonNull::new(heap) {
        None => core::ptr::null_mut(),
        // SAFETY: live heap per contract.
        Some(h) => out(unsafe { h.as_ref() }.alloc_aligned(size, alignment.max(1))),
    }
}

/// `mi_heap_realloc`.
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_realloc(
    heap: *mut Heap,
    p: *mut c_void,
    newsize: usize,
) -> *mut c_void {
    let Some(nn) = NonNull::new(p as *mut u8) else {
        // SAFETY: forwarded contract.
        return unsafe { mi_heap_malloc(heap, newsize) };
    };
    // SAFETY: live allocation.
    let old = unsafe { heap::usable_size(nn) };
    if newsize <= old {
        return p;
    }
    // SAFETY: forwarded contract.
    let np = unsafe { mi_heap_malloc(heap, newsize) };
    if !np.is_null() {
        // SAFETY: valid disjoint regions of `min(old, newsize)` bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(p as *const u8, np as *mut u8, old.min(newsize));
            init::free(nn);
        }
    }
    np
}

/// `mi_heap_mallocn`: allocate `count * size` bytes from `heap` (overflow ⇒ null).
///
/// # Safety
/// As [`mi_heap_malloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_heap_mallocn(
    heap: *mut Heap,
    count: usize,
    size: usize,
) -> *mut c_void {
    match checked_total(count, size) {
        // SAFETY: forwarded contract.
        Some(total) => unsafe { mi_heap_malloc(heap, total) },
        None => core::ptr::null_mut(),
    }
}

/// `mi_heap_reallocn`: resize `p` to `count * size` in `heap` (overflow ⇒ null).
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_reallocn(
    heap: *mut Heap,
    p: *mut c_void,
    count: usize,
    size: usize,
) -> *mut c_void {
    match checked_total(count, size) {
        // SAFETY: forwarded contract.
        Some(total) => unsafe { mi_heap_realloc(heap, p, total) },
        None => core::ptr::null_mut(),
    }
}

/// `mi_heap_reallocf` (BSD `reallocf`): resize `p` to `newsize` in `heap`; on
/// failure the original `p` is freed before returning null.
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_reallocf(
    heap: *mut Heap,
    p: *mut c_void,
    newsize: usize,
) -> *mut c_void {
    // SAFETY: forwarded contract.
    let np = unsafe { mi_heap_realloc(heap, p, newsize) };
    if np.is_null() && !p.is_null() {
        // SAFETY: realloc failed and did not free `p`; free the original.
        unsafe { mi_free(p) };
    }
    np
}

/// `mi_heap_strdup`: duplicate a NUL-terminated C string, allocating from `heap`.
///
/// # Safety
/// `heap` is live; `s` is null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_strdup(heap: *mut Heap, s: *const c_char) -> *mut c_char {
    if s.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `s` is a valid C string per contract.
    let len = unsafe { c_strlen(s) };
    // SAFETY: forwarded contract.
    let p = unsafe { mi_heap_malloc(heap, len + 1) };
    if p.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: dst valid for len+1; src valid for len+1 (incl NUL).
    unsafe { core::ptr::copy_nonoverlapping(s as *const u8, p as *mut u8, len + 1) };
    p as *mut c_char
}

/// `mi_heap_strndup`: duplicate at most `n` bytes of a C string, from `heap`.
///
/// # Safety
/// `heap` is live; `s` is null or points to at least `min(strlen(s), n)`
/// readable bytes.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_strndup(
    heap: *mut Heap,
    s: *const c_char,
    n: usize,
) -> *mut c_char {
    if s.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `s` valid per contract.
    let len = unsafe { c_strnlen(s, n) };
    // SAFETY: forwarded contract.
    let p = unsafe { mi_heap_malloc(heap, len + 1) };
    if p.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: copy `len` bytes then NUL-terminate.
    unsafe {
        core::ptr::copy_nonoverlapping(s as *const u8, p as *mut u8, len);
        *(p as *mut u8).add(len) = 0;
    }
    p as *mut c_char
}

/// Allocate from `heap` so that `ptr + offset` is `align`-aligned.
///
/// # Safety
/// `heap` is a live heap from [`mi_heap_new`], used from its owning thread.
unsafe fn heap_aligned_at(
    heap: *mut Heap,
    size: usize,
    align: usize,
    offset: usize,
) -> Option<NonNull<u8>> {
    let h = NonNull::new(heap)?;
    if !align.is_power_of_two() {
        return None;
    }
    // SAFETY: live heap per contract.
    let heap = unsafe { h.as_ref() };
    if offset == 0 {
        return heap.alloc_aligned(size, align.max(1));
    }
    // Over-allocate so an interior pointer `r` with `(r+offset) % align == 0`
    // fits; `free` recovers the block start via the page-map. Guard against
    // `size + align` overflow (return null rather than wrap).
    let p = heap.alloc(size.checked_add(align)?)?;
    let base = p.addr().get();
    let r = base + ((align - ((base + offset) & (align - 1))) & (align - 1));
    // SAFETY: `r` is within `[base, base+align)` and the block is ≥ size+align.
    Some(unsafe { NonNull::new_unchecked(p.as_ptr().with_addr(r)) })
}

/// `mi_heap_malloc_aligned_at`.
///
/// # Safety
/// As [`mi_heap_malloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_heap_malloc_aligned_at(
    heap: *mut Heap,
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    // SAFETY: live heap per contract.
    out(unsafe { heap_aligned_at(heap, size, alignment, offset) })
}

/// `mi_heap_zalloc_aligned`.
///
/// # Safety
/// As [`mi_heap_malloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_heap_zalloc_aligned(
    heap: *mut Heap,
    size: usize,
    alignment: usize,
) -> *mut c_void {
    // SAFETY: live heap per contract.
    let p = unsafe { heap_aligned_at(heap, size, alignment, 0) };
    if let Some(nn) = p {
        // SAFETY: valid for `size` bytes.
        unsafe { core::ptr::write_bytes(nn.as_ptr(), 0, size) };
    }
    out(p)
}

/// `mi_heap_zalloc_aligned_at`.
///
/// # Safety
/// As [`mi_heap_malloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_heap_zalloc_aligned_at(
    heap: *mut Heap,
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    // SAFETY: live heap per contract.
    let p = unsafe { heap_aligned_at(heap, size, alignment, offset) };
    if let Some(nn) = p {
        // SAFETY: valid for `size` bytes.
        unsafe { core::ptr::write_bytes(nn.as_ptr(), 0, size) };
    }
    out(p)
}

/// `mi_heap_calloc_aligned` (overflow ⇒ null).
///
/// # Safety
/// As [`mi_heap_malloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_heap_calloc_aligned(
    heap: *mut Heap,
    count: usize,
    size: usize,
    alignment: usize,
) -> *mut c_void {
    match checked_total(count, size) {
        // SAFETY: forwarded contract.
        Some(total) => unsafe { mi_heap_zalloc_aligned(heap, total, alignment) },
        None => core::ptr::null_mut(),
    }
}

/// `mi_heap_calloc_aligned_at` (overflow ⇒ null).
///
/// # Safety
/// As [`mi_heap_malloc`].
#[no_mangle]
pub unsafe extern "C" fn mi_heap_calloc_aligned_at(
    heap: *mut Heap,
    count: usize,
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    match checked_total(count, size) {
        // SAFETY: forwarded contract.
        Some(total) => unsafe { mi_heap_zalloc_aligned_at(heap, total, alignment, offset) },
        None => core::ptr::null_mut(),
    }
}

/// `mi_heap_realloc_aligned`: resize `p` to `newsize` in `heap`, keeping the
/// result `alignment`-aligned. `Heap` has no in-place aligned realloc, so this
/// allocates a fresh aligned block, copies, and frees the old block.
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_realloc_aligned(
    heap: *mut Heap,
    p: *mut c_void,
    newsize: usize,
    alignment: usize,
) -> *mut c_void {
    let Some(nn) = NonNull::new(p as *mut u8) else {
        // SAFETY: live heap per contract.
        return out(unsafe { heap_aligned_at(heap, newsize, alignment, 0) });
    };
    // SAFETY: live allocation.
    let old = unsafe { heap::usable_size(nn) };
    // In-place reuse: if the block already fits and is still correctly aligned,
    // return it unchanged (matches the C reference and keeps the pointer stable).
    if newsize <= old
        && alignment.max(1).is_power_of_two()
        && nn.addr().get() % alignment.max(1) == 0
    {
        return p;
    }
    // SAFETY: live heap per contract.
    let Some(np) = (unsafe { heap_aligned_at(heap, newsize, alignment, 0) }) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `np`/`nn` are valid for `min(old, newsize)` bytes and disjoint.
    unsafe {
        core::ptr::copy_nonoverlapping(nn.as_ptr(), np.as_ptr(), old.min(newsize));
        init::free(nn);
    }
    out(Some(np))
}

/// `mi_heap_realloc_aligned_at`: like [`mi_heap_realloc_aligned`] but the result
/// is `alignment`-aligned at `offset`.
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_realloc_aligned_at(
    heap: *mut Heap,
    p: *mut c_void,
    newsize: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    let Some(nn) = NonNull::new(p as *mut u8) else {
        // SAFETY: live heap per contract.
        return out(unsafe { heap_aligned_at(heap, newsize, alignment, offset) });
    };
    // SAFETY: live allocation.
    let old = unsafe { heap::usable_size(nn) };
    // In-place reuse: if the block already fits and `p + offset` is still
    // correctly aligned, return it unchanged (matches the C reference).
    if newsize <= old
        && alignment.max(1).is_power_of_two()
        && (nn.addr().get() + offset) % alignment.max(1) == 0
    {
        return p;
    }
    // SAFETY: live heap per contract.
    let Some(np) = (unsafe { heap_aligned_at(heap, newsize, alignment, offset) }) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `np`/`nn` are valid for `min(old, newsize)` bytes and disjoint.
    unsafe {
        core::ptr::copy_nonoverlapping(nn.as_ptr(), np.as_ptr(), old.min(newsize));
        init::free(nn);
    }
    out(Some(np))
}

/// `mi_heap_rezalloc`: resize `p` to `newsize` in `heap`, zeroing any grown tail.
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_rezalloc(
    heap: *mut Heap,
    p: *mut c_void,
    newsize: usize,
) -> *mut c_void {
    // SAFETY: `p` is null or live.
    let old = NonNull::new(p as *mut u8).map_or(0, |nn| unsafe { heap::usable_size(nn) });
    // SAFETY: forwarded contract.
    let np = unsafe { mi_heap_realloc(heap, p, newsize) };
    if !np.is_null() && newsize > old {
        // SAFETY: `np` is valid for `newsize` bytes; zero the grown tail.
        unsafe { core::ptr::write_bytes((np as *mut u8).add(old), 0, newsize - old) };
    }
    np
}

/// `mi_heap_recalloc`: resize `p` to `newcount * size` in `heap` (overflow ⇒
/// null), zeroing any grown tail.
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_recalloc(
    heap: *mut Heap,
    p: *mut c_void,
    newcount: usize,
    size: usize,
) -> *mut c_void {
    match checked_total(newcount, size) {
        // SAFETY: forwarded contract.
        Some(total) => unsafe { mi_heap_rezalloc(heap, p, total) },
        None => core::ptr::null_mut(),
    }
}

/// `mi_heap_rezalloc_aligned`: resize `p` to `newsize` in `heap`, keeping the
/// result `alignment`-aligned and zeroing any grown tail.
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_rezalloc_aligned(
    heap: *mut Heap,
    p: *mut c_void,
    newsize: usize,
    alignment: usize,
) -> *mut c_void {
    // SAFETY: `p` is null or live.
    let old = NonNull::new(p as *mut u8).map_or(0, |nn| unsafe { heap::usable_size(nn) });
    // SAFETY: forwarded contract.
    let np = unsafe { mi_heap_realloc_aligned(heap, p, newsize, alignment) };
    if !np.is_null() && newsize > old {
        // SAFETY: `np` is valid for `newsize` bytes; zero the grown tail.
        unsafe { core::ptr::write_bytes((np as *mut u8).add(old), 0, newsize - old) };
    }
    np
}

/// `mi_heap_rezalloc_aligned_at`: like [`mi_heap_rezalloc_aligned`] but the
/// result is `alignment`-aligned at `offset`.
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_rezalloc_aligned_at(
    heap: *mut Heap,
    p: *mut c_void,
    newsize: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    // SAFETY: `p` is null or live.
    let old = NonNull::new(p as *mut u8).map_or(0, |nn| unsafe { heap::usable_size(nn) });
    // SAFETY: forwarded contract.
    let np = unsafe { mi_heap_realloc_aligned_at(heap, p, newsize, alignment, offset) };
    if !np.is_null() && newsize > old {
        // SAFETY: `np` is valid for `newsize` bytes; zero the grown tail.
        unsafe { core::ptr::write_bytes((np as *mut u8).add(old), 0, newsize - old) };
    }
    np
}

/// `mi_heap_recalloc_aligned`: resize `p` to `newcount * size` in `heap`
/// (overflow ⇒ null), keeping the result `alignment`-aligned and zeroing any
/// grown tail.
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_recalloc_aligned(
    heap: *mut Heap,
    p: *mut c_void,
    newcount: usize,
    size: usize,
    alignment: usize,
) -> *mut c_void {
    match checked_total(newcount, size) {
        // SAFETY: forwarded contract.
        Some(total) => unsafe { mi_heap_rezalloc_aligned(heap, p, total, alignment) },
        None => core::ptr::null_mut(),
    }
}

/// `mi_heap_recalloc_aligned_at`: like [`mi_heap_recalloc_aligned`] but the
/// result is `alignment`-aligned at `offset`.
///
/// # Safety
/// `heap` is live; `p` is null or a live allocation.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_recalloc_aligned_at(
    heap: *mut Heap,
    p: *mut c_void,
    newcount: usize,
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    match checked_total(newcount, size) {
        // SAFETY: forwarded contract.
        Some(total) => unsafe { mi_heap_rezalloc_aligned_at(heap, p, total, alignment, offset) },
        None => core::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// `mi_collect`: reclaim memory in the default heap; `force!=0` is aggressive.
#[no_mangle]
pub extern "C" fn mi_collect(force: bool) {
    init::collect(force);
}

/// `mi_heap_collect`: reclaim memory in `heap`.
///
/// # Safety
/// `heap` is a live heap from [`mi_heap_new`], used only from its owning thread.
#[no_mangle]
pub unsafe extern "C" fn mi_heap_collect(heap: *mut Heap, force: bool) {
    if let Some(h) = NonNull::new(heap) {
        // SAFETY: live owner-thread heap per contract.
        unsafe { h.as_ref() }.collect(force);
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

// `c_int` is `i32` and (on the supported Linux target) `c_long` is `i64`, so
// the option index/value pass through without casts.

/// `mi_option_get`: current value of option `option` (0 for unknown).
#[no_mangle]
pub extern "C" fn mi_option_get(option: c_int) -> c_long {
    Opt::from_index(option).map_or(0, options::get)
}

/// `mi_option_set`.
#[no_mangle]
pub extern "C" fn mi_option_set(option: c_int, value: c_long) {
    if let Some(o) = Opt::from_index(option) {
        options::set(o, value);
    }
}

/// `mi_option_is_enabled` (non-zero).
#[no_mangle]
pub extern "C" fn mi_option_is_enabled(option: c_int) -> c_int {
    Opt::from_index(option).is_some_and(options::is_enabled) as c_int
}

/// `mi_option_enable`.
#[no_mangle]
pub extern "C" fn mi_option_enable(option: c_int) {
    if let Some(o) = Opt::from_index(option) {
        options::enable(o);
    }
}

/// `mi_option_disable`.
#[no_mangle]
pub extern "C" fn mi_option_disable(option: c_int) {
    if let Some(o) = Opt::from_index(option) {
        options::disable(o);
    }
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// `mi_stats_reset`.
#[no_mangle]
pub extern "C" fn mi_stats_reset() {
    stats::reset();
}

/// `mi_stats_merge`: single global stats — nothing to merge.
#[no_mangle]
pub extern "C" fn mi_stats_merge() {}

/// `mi_stats_print`: print process statistics to stderr (the `out` sink is
/// ignored — custom output sinks are follow-up work).
///
/// # Safety
/// Trivially safe; `_out` is ignored.
#[no_mangle]
pub unsafe extern "C" fn mi_stats_print(_out: *mut c_void) {
    let s = stats::snapshot();
    // Build a small message without heap allocation.
    let mut buf = [0u8; 256];
    let msg = format_stats(&s, &mut buf);
    <crate::prim::DefaultPrim as crate::prim::Prim>::out_stderr(msg);
}

/// Format a stats line into `buf`, returning the written `&str` (no allocation).
fn format_stats<'a>(s: &stats::Stats, buf: &'a mut [u8]) -> &'a str {
    let mut w = Writer { buf, pos: 0 };
    use core::fmt::Write as _;
    let _ = writeln!(
        w,
        "mimalloc-rs stats: allocs={} frees={} live={}B peak={}B pages={}",
        s.allocations, s.frees, s.current_bytes, s.peak_bytes, s.pages_created
    );
    let pos = w.pos;
    // SAFETY: only ASCII written by `write!` above.
    core::str::from_utf8(&w.buf[..pos]).unwrap_or("mimalloc-rs stats\n")
}

struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}
impl core::fmt::Write for Writer<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let b = s.as_bytes();
        let n = b.len().min(self.buf.len() - self.pos);
        self.buf[self.pos..self.pos + n].copy_from_slice(&b[..n]);
        self.pos += n;
        Ok(())
    }
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

            // overflow guard: `size + align` must not wrap to a tiny block.
            assert!(mi_malloc_aligned(usize::MAX, 16).is_null());
            assert!(mi_malloc_aligned_at(usize::MAX, 64, 8).is_null());
            assert!(mi_aligned_alloc(64, usize::MAX).is_null());
            assert_eq!(mi_posix_memalign(&mut pp, 16, usize::MAX), ENOMEM);
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

    #[test]
    fn c_api_first_class_heap() {
        // SAFETY: standard mi_heap_* usage on one thread.
        unsafe {
            let h = mi_heap_new();
            assert!(!h.is_null());
            let mut ptrs = std::vec::Vec::new();
            for i in 0..500 {
                let p = mi_heap_malloc(h, 64);
                assert!(!p.is_null());
                core::ptr::write_bytes(p as *mut u8, (i & 0xff) as u8, 64);
                ptrs.push(p);
            }
            let z = mi_heap_zalloc(h, 128);
            assert_eq!(*(z as *const u8), 0);
            let a = mi_heap_malloc_aligned(h, 40, 64);
            assert_eq!(a.addr() % 64, 0);
            // destroy frees ALL blocks of the heap at once (no per-block free)
            mi_heap_destroy(h);

            // a second heap, deleted (blocks would stay valid if outstanding)
            let h2 = mi_heap_new();
            let q = mi_heap_malloc(h2, 32);
            mi_free(q); // free before delete so nothing is abandoned
            mi_heap_delete(h2);
        }
    }

    #[test]
    fn c_api_heap_variants() {
        // SAFETY: standard mi_heap_* usage on one thread.
        unsafe {
            let h = mi_heap_new();
            assert!(!h.is_null());

            // mi_heap_mallocn: count*size, overflow guarded.
            let n = mi_heap_mallocn(h, 8, 16);
            assert!(!n.is_null());
            assert!(mi_usable_size(n) >= 128);
            assert!(mi_heap_mallocn(h, usize::MAX, 2).is_null());

            // mi_heap_malloc_aligned_at: (ptr+offset) aligned.
            let at = mi_heap_malloc_aligned_at(h, 64, 64, 8);
            assert!(!at.is_null());
            assert_eq!(((at as usize) + 8) % 64, 0);

            // mi_heap_zalloc_aligned: aligned + zeroed.
            let za = mi_heap_zalloc_aligned(h, 40, 128);
            assert!(!za.is_null());
            assert_eq!((za as usize) % 128, 0);
            assert_eq!(*(za as *const u8), 0);

            // mi_heap_calloc_aligned: aligned + zeroed; overflow guarded.
            let ca = mi_heap_calloc_aligned(h, 4, 32, 64) as *mut u8;
            assert!(!ca.is_null());
            assert_eq!((ca as usize) % 64, 0);
            assert_eq!(*ca.add(64), 0);
            assert!(mi_heap_calloc_aligned(h, usize::MAX, 2, 16).is_null());

            // mi_heap_realloc_aligned: pattern preserved across a grow.
            let r0 = mi_heap_realloc_aligned(h, core::ptr::null_mut(), 32, 64) as *mut u8;
            assert!(!r0.is_null());
            core::ptr::write_bytes(r0, 0x5A, 32);
            let r1 = mi_heap_realloc_aligned(h, r0 as *mut c_void, 4096, 64) as *mut u8;
            assert!(!r1.is_null());
            assert_eq!((r1 as usize) % 64, 0);
            for i in 0..32 {
                assert_eq!(*r1.add(i), 0x5A);
            }

            // mi_heap_recalloc: grown tail zeroed.
            let c0 = mi_heap_malloc(h, 16);
            core::ptr::write_bytes(c0 as *mut u8, 0xFF, 16);
            let c1 = mi_heap_recalloc(h, c0, 1, 4096) as *mut u8;
            assert!(!c1.is_null());
            assert_eq!(*c1.add(2048), 0);

            // mi_heap_strdup.
            let d = mi_heap_strdup(h, c"hello".as_ptr());
            assert!(!d.is_null());
            assert_eq!(c_strlen(d), 5);

            // mi_heap_reallocf: basic grow works.
            let f0 = mi_heap_malloc(h, 8);
            let f1 = mi_heap_reallocf(h, f0, 64);
            assert!(!f1.is_null());
            assert!(mi_usable_size(f1) >= 64);

            // mi_heap_reallocn / mi_heap_strndup.
            let rn = mi_heap_reallocn(h, core::ptr::null_mut(), 4, 32);
            assert!(!rn.is_null() && mi_usable_size(rn) >= 128);
            let sn = mi_heap_strndup(h, c"hello world".as_ptr(), 5);
            assert_eq!(c_strlen(sn), 5);

            // Offset-aligned resize path (the most intricate logic): allocate at
            // an offset, write a pattern, grow via rezalloc_aligned_at, and assert
            // the pattern is preserved, the new offset alignment holds, and the
            // grown tail is zeroed.
            let o0 = mi_heap_malloc_aligned_at(h, 48, 64, 8) as *mut u8;
            assert_eq!((o0 as usize + 8) % 64, 0);
            core::ptr::write_bytes(o0, 0x3C, 48);
            let o1 = mi_heap_rezalloc_aligned_at(h, o0 as *mut c_void, 4096, 64, 8) as *mut u8;
            assert!(!o1.is_null());
            assert_eq!((o1 as usize + 8) % 64, 0);
            for i in 0..48 {
                assert_eq!(*o1.add(i), 0x3C, "offset rezalloc lost data");
            }
            assert_eq!(*o1.add(2048), 0, "grown tail not zeroed");

            // In-place reuse: shrinking an aligned block returns the same pointer.
            let s0 = mi_heap_malloc_aligned(h, 256, 64) as *mut u8;
            let s1 = mi_heap_realloc_aligned(h, s0 as *mut c_void, 64, 64) as *mut u8;
            assert_eq!(s0, s1, "in-place shrink should keep the pointer");

            // overflow guards on aligned paths.
            assert!(mi_heap_malloc_aligned_at(h, usize::MAX, 64, 8).is_null());
            assert!(mi_heap_calloc_aligned(h, usize::MAX, 2, 16).is_null());
            assert!(
                mi_heap_recalloc_aligned(h, core::ptr::null_mut(), usize::MAX, 2, 64).is_null()
            );

            // Free all blocks at once via destroy (recovers offset-path blocks).
            mi_heap_destroy(h);
        }
    }

    #[test]
    fn c_api_collect() {
        // SAFETY: standard C-style usage on one thread.
        unsafe {
            // Default-heap collect: alloc, free, then reclaim.
            let mut ps = std::vec::Vec::new();
            for _ in 0..500 {
                ps.push(mi_malloc(64));
            }
            for p in ps.drain(..) {
                mi_free(p);
            }
            mi_collect(true);

            // First-class heap collect.
            let h = mi_heap_new();
            assert!(!h.is_null());
            let mut hp = std::vec::Vec::new();
            for _ in 0..500 {
                hp.push(mi_heap_malloc(h, 48));
            }
            for p in hp.drain(..) {
                mi_free(p);
            }
            mi_heap_collect(h, false);
            mi_heap_delete(h);

            // null heap is a no-op.
            mi_heap_collect(core::ptr::null_mut(), true);

            // Allocator still usable after collection.
            let q = mi_malloc(100);
            assert!(!q.is_null());
            mi_free(q);
        }
    }

    #[test]
    fn c_api_options() {
        let opt = Opt::PurgeDelay as c_int;
        let prev = mi_option_get(opt);
        mi_option_set(opt, 1234);
        assert_eq!(mi_option_get(opt), 1234);
        mi_option_set(opt, prev);

        let v = Opt::Verbose as c_int;
        mi_option_disable(v);
        assert_eq!(mi_option_is_enabled(v), 0);
        mi_option_enable(v);
        assert_eq!(mi_option_is_enabled(v), 1);
        mi_option_disable(v);

        mi_stats_reset();
        // SAFETY: out sink ignored.
        unsafe { mi_stats_print(core::ptr::null_mut()) };
    }

    #[test]
    fn c_api_valloc_pvalloc() {
        let ps = crate::os::page_size();
        // SAFETY: standard usage.
        unsafe {
            let v = mi_valloc(100);
            assert!(!v.is_null());
            assert_eq!(v.addr() % ps, 0);
            mi_free(v);

            let pv = mi_pvalloc(100);
            assert!(!pv.is_null());
            assert_eq!(pv.addr() % ps, 0);
            // rounded up to at least one page
            assert!(mi_usable_size(pv) >= ps);
            mi_free(pv);

            // overflow guard
            assert!(mi_pvalloc(usize::MAX).is_null());
        }
    }

    #[test]
    fn c_api_reallocarray() {
        // SAFETY: standard usage.
        unsafe {
            let p = mi_reallocarray(core::ptr::null_mut(), 4, 16);
            assert!(!p.is_null());
            assert!(mi_usable_size(p) >= 64);
            mi_free(p);

            // overflow ⇒ null and errno = EOVERFLOW
            let o = mi_reallocarray(core::ptr::null_mut(), usize::MAX, 2);
            assert!(o.is_null());
        }
    }

    #[test]
    fn c_api_reallocarr() {
        // SAFETY: standard NetBSD `reallocarr` usage.
        unsafe {
            let mut p: *mut c_void = core::ptr::null_mut();
            let pp = &mut p as *mut _ as *mut c_void;

            assert_eq!(mi_reallocarr(pp, 8, 16), 0);
            assert!(!p.is_null());
            assert!(mi_usable_size(p) >= 128);

            // count==0 frees and nulls the pointer
            assert_eq!(mi_reallocarr(pp, 0, 16), 0);
            assert!(p.is_null());

            // null ptrp ⇒ EINVAL
            assert_eq!(mi_reallocarr(core::ptr::null_mut(), 1, 1), EINVAL);
            // overflow ⇒ EOVERFLOW
            assert_eq!(mi_reallocarr(pp, usize::MAX, 2), EOVERFLOW);
        }
    }

    #[test]
    fn c_api_aligned_recalloc_zeroes() {
        // SAFETY: standard usage.
        unsafe {
            // fresh aligned alloc, zeroed
            let p = mi_aligned_recalloc(core::ptr::null_mut(), 1, 4096, 64) as *mut u8;
            assert!(!p.is_null());
            assert_eq!((p as usize) % 64, 0);
            assert_eq!(*p.add(2048), 0);
            mi_free(p as *mut c_void);

            // grow an existing block; grown tail must be zeroed
            let q = mi_malloc(16);
            core::ptr::write_bytes(q as *mut u8, 0xFF, 16);
            let q2 = mi_aligned_recalloc(q, 1, 4096, 64) as *mut u8;
            assert_eq!((q2 as usize) % 64, 0);
            assert_eq!(*q2.add(2048), 0);
            mi_free(q2 as *mut c_void);

            // mi_recalloc_aligned is the same; mi_recalloc_aligned_at honors offset
            let r = mi_recalloc_aligned(core::ptr::null_mut(), 2, 64, 32) as *mut u8;
            assert_eq!((r as usize) % 32, 0);
            assert_eq!(*r.add(64), 0);
            mi_free(r as *mut c_void);

            let s = mi_recalloc_aligned_at(core::ptr::null_mut(), 1, 256, 64, 8);
            assert_eq!(((s as usize) + 8) % 64, 0);
            mi_free(s);
        }
    }

    #[test]
    fn c_api_size_aliases_and_cfree() {
        // SAFETY: standard usage.
        unsafe {
            let p = mi_malloc(100);
            assert!(!p.is_null());
            assert!(mi_malloc_usable_size(p as *const c_void) >= 100);
            assert_eq!(
                mi_malloc_size(p as *const c_void),
                mi_malloc_usable_size(p as *const c_void)
            );
            mi_cfree(p);

            assert_eq!(mi_malloc_usable_size(core::ptr::null()), 0);
            assert!(mi_malloc_good_size(1) >= 1);

            let a = mi_malloc_aligned(40, 128);
            mi_free_size_aligned(a, 40, 128);
        }
    }
}
