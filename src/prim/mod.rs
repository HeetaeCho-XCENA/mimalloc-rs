// SPDX-License-Identifier: MIT
//! The primitive portability layer: the OS abstraction every platform must
//! implement (ports `include/mimalloc/prim.h`).
//!
//! mimalloc selects a single `prim` implementation at compile time. We mirror
//! that with the [`Prim`] trait, whose methods are *associated* (stateless), and
//! a per-platform implementor re-exported as [`DefaultPrim`].
//!
//! The trait covers raw OS memory management (reserve/commit/decommit/reset/
//! protect/free) plus the small system queries the allocator needs (page
//! config, NUMA, clock, randomness, env, yield, stderr). Thread-local storage
//! and the thread-exit hook (`_mi_prim_thread_init_auto_done`) are layered in
//! at M5 where the heap lifecycle lives.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::Sys as DefaultPrim;

#[cfg(not(target_os = "linux"))]
compile_error!("mimalloc-rs currently supports Linux only (Windows/macOS prim are follow-up work)");

/// An OS error, carrying the platform `errno`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PrimError(pub i32);

/// Result of a primitive operation.
pub type PrimResult<T> = Result<T, PrimError>;

/// OS memory configuration, queried once at startup (`mi_os_mem_config_t`).
#[derive(Clone, Copy, Debug)]
pub struct OsMemConfig {
    /// OS page size (typically 4 KiB).
    pub page_size: usize,
    /// Large/huge page size, or 0 if unsupported (typically 2 MiB).
    pub large_page_size: usize,
    /// Smallest allocation granularity (4 KiB on Linux, 64 KiB on Windows).
    pub alloc_granularity: usize,
    /// Physical memory in bytes (0 if unknown).
    pub physical_memory: usize,
    /// Usable virtual address bits (≈47 on x64).
    pub virtual_address_bits: usize,
    /// Can we reserve more than can be committed?
    pub has_overcommit: bool,
    /// Can allocated regions be freed partially? (true for `mmap`).
    pub has_partial_free: bool,
    /// Supports reserving virtual address space without committing.
    pub has_virtual_reserve: bool,
    /// Transparent huge pages enabled (Linux).
    pub has_transparent_huge_pages: bool,
}

/// Successful raw allocation.
#[derive(Clone, Copy, Debug)]
pub struct PrimAlloc {
    /// Base address of the mapping (page aligned, not necessarily `try_align`).
    pub addr: *mut u8,
    /// Whether large/huge OS pages backed the allocation.
    pub is_large: bool,
    /// Whether the memory is known to be zero-initialized.
    pub is_zero: bool,
}

/// The compile-time-selected OS primitive interface.
///
/// All methods are associated functions: the implementor is a zero-sized type.
/// Memory-touching methods are `unsafe` because the caller must pass page
/// aligned, non-overlapping, owned `(addr, size)` ranges.
pub trait Prim {
    /// Query the OS memory configuration (called once, then cached by `os`).
    fn mem_config() -> OsMemConfig;

    /// Reserve (or commit) `size` bytes of virtual memory.
    ///
    /// When `commit` is false, the range is only reserved (no access) and must
    /// be committed later. `try_align` is a hint only.
    ///
    /// # Safety
    /// `size` must be > 0 and page aligned; `try_align` a power of two ≥ page size.
    unsafe fn alloc(
        hint: *mut u8,
        size: usize,
        try_align: usize,
        commit: bool,
        allow_large: bool,
    ) -> PrimResult<PrimAlloc>;

    /// Release a previously allocated range back to the OS.
    ///
    /// # Safety
    /// `(addr, size)` must denote a range previously returned by [`Prim::alloc`]
    /// (or a sub-range on platforms with partial-free).
    unsafe fn free(addr: *mut u8, size: usize) -> PrimResult<()>;

    /// Commit (make accessible) a previously reserved range. Returns whether
    /// the committed memory is zero.
    ///
    /// # Safety
    /// `(addr, size)` must be page aligned and within a reserved mapping.
    unsafe fn commit(addr: *mut u8, size: usize) -> PrimResult<bool>;

    /// Decommit a range (return physical pages). Returns `needs_recommit`:
    /// whether the range must be explicitly re-committed before reuse.
    ///
    /// # Safety
    /// `(addr, size)` must be page aligned and committed.
    unsafe fn decommit(addr: *mut u8, size: usize) -> PrimResult<bool>;

    /// Reset a range: contents may be discarded, but it stays accessible.
    ///
    /// # Safety
    /// `(addr, size)` must be page aligned and committed.
    unsafe fn reset(addr: *mut u8, size: usize) -> PrimResult<()>;

    /// Notify the OS a previously reset/decommitted range is being reused.
    /// A no-op on Linux.
    ///
    /// # Safety
    /// `(addr, size)` must be page aligned and committed.
    unsafe fn reuse(addr: *mut u8, size: usize) -> PrimResult<()>;

    /// Protect (`protect = true` ⇒ no access) or unprotect a range.
    ///
    /// # Safety
    /// `(addr, size)` must be page aligned and within an owned mapping.
    unsafe fn protect(addr: *mut u8, size: usize, protect: bool) -> PrimResult<()>;

    /// Current thread's NUMA node.
    fn numa_node() -> usize {
        0
    }

    /// Number of NUMA nodes on the system.
    fn numa_node_count() -> usize {
        1
    }

    /// Monotonic clock in milliseconds.
    fn clock_now_msecs() -> i64;

    /// Fill `buf` with cryptographically strong randomness; `false` on failure.
    fn random_buf(buf: &mut [u8]) -> bool;

    /// Read environment variable `name` into `out`; returns bytes written.
    fn getenv(name: &str, out: &mut [u8]) -> Option<usize>;

    /// Yield to other threads (≈ `sleep(0)`).
    fn thread_yield();

    /// Write a diagnostic message to stderr.
    fn out_stderr(msg: &str);
}
