// SPDX-License-Identifier: MIT
//! Thread-heaps (ports `src/theap.c` / `src/threadlocal.c`).
//!
//! In mimalloc v3 a `mi_theap_t` is the *thread-local* view of a `mi_heap_t`
//! together with its thread-local data (`tld`) and a monotonic `heartbeat`
//! driving deferred frees. v1 of this port keeps the thread heap as a plain
//! [`Heap`] stored directly in thread-local storage (see [`crate::init`]).
//! Abandoned-page handoff (on thread exit) and the deferred-free heartbeat are
//! implemented on [`Heap`]; only the dedicated `mi_theap_t`/`tld` split is still
//! future work.

pub use crate::heap::Heap;

/// The thread-local heap type. Currently an alias for [`Heap`]; kept as a
/// distinct name so the `tld`/heartbeat split can grow here without churn.
pub type Theap = Heap;
