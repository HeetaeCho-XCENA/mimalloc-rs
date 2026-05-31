// SPDX-License-Identifier: MIT
//! Memory-tracking hooks (ports `include/mimalloc/track.h`).
//!
//! mimalloc annotates allocations for Valgrind/ASan/ETW. This port defines the
//! hook surface as zero-cost no-ops; wiring them to a real tracker (Valgrind
//! client requests, ASan poisoning) is follow-up work behind the `track` feature.

/// Mark `[p, p+size)` as freshly allocated and defined.
#[inline]
pub fn track_mem_defined(_p: *const u8, _size: usize) {
    // no-op (see module docs)
}

/// Mark `[p, p+size)` as undefined (allocated, uninitialized).
#[inline]
pub fn track_mem_undefined(_p: *const u8, _size: usize) {}

/// Mark `[p, p+size)` as no-access (freed).
#[inline]
pub fn track_mem_noaccess(_p: *const u8, _size: usize) {}
