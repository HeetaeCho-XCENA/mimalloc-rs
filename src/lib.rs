// SPDX-License-Identifier: MIT
// Copyright (c) 2018-2024 Microsoft Research, Daan Leijen (original C mimalloc)
// Copyright (c) 2026 mimalloc-rs contributors (Rust port)

//! # mimalloc-rs
//!
//! A from-scratch Rust re-implementation of the **mimalloc v3** general-purpose
//! allocator (`MI_MALLOC_VERSION 30302`). This is *not* an FFI binding: the
//! allocation engine — free-list sharding, the segment-less arena+bitmap design
//! that manages pages directly, the `thread_free` MPSC list, and the
//! security-by-design layout — is ported to idiomatic Rust.
//!
//! ## Layering
//!
//! ```text
//!   subproc → arena (64 KiB slices, atomic bitmap) → page → block
//!                                  │
//!                            page-map (address → page)
//! ```
//!
//! Memory comes from the OS through the [`prim`] abstraction (Linux-first).
//! The core is `#![no_std]`; the `std` feature (on by default) provides OS
//! threads and `thread_local!`-based TLS.
//!
//! ## Unsafe policy
//!
//! `#![forbid(unsafe_code)]` is impossible for an allocator. Instead:
//! * `unsafe` is confined to the low-level cores (`atomic`, `bitmap`,
//!   `free_list`, `page_map`, `os`, `prim`) and the public API shims.
//! * Every `unsafe` block carries a `// SAFETY:` comment justifying it.
//! * The allocation fast paths are **panic-free**: out-of-memory returns
//!   `None`/null; internal invariant violations `abort()` rather than unwind.
//! * Pointer/integer round-trips use the strict-provenance APIs
//!   (`with_addr`, `map_addr`, `expose_provenance`, `with_exposed_provenance`).

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(feature = "nightly", feature(allocator_api))]
#![cfg_attr(feature = "nightly", feature(thread_local))]

// The core never *requires* `alloc`, but host-side helpers and tests may use it.
#[cfg(feature = "std")]
extern crate std;

pub mod api;
pub mod arena;
pub mod arena_meta;
pub mod atomic;
pub mod bitmap;
pub mod bits;
pub mod free_list;
pub mod heap;
pub mod init;
pub mod layout;
pub mod options;
pub mod os;
pub mod page;
pub mod page_map;
pub mod page_queue;
pub mod prim;
pub mod stats;
pub mod subproc;
pub mod sync;
pub mod track;

pub use api::MiMalloc;
pub use bits::{MI_MALLOC_VERSION, MI_MALLOC_VERSION_STRING};
pub use heap::Heap;
pub use init::free;
#[cfg(feature = "std")]
pub use init::{malloc, malloc_aligned, realloc, zalloc};
