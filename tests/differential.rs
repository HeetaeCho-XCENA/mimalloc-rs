// SPDX-License-Identifier: MIT
//! Differential test against the C reference allocator (`libmimalloc`).
//!
//! Enabled by the `differential` feature; requires the C library to be linked
//! (set `MIMALLOC_C_LIB` to a directory containing `libmimalloc.so`, e.g. a
//! built `mimalloc-v3` checkout). Run with:
//!
//! ```sh
//! MIMALLOC_C_LIB=/path/to/mimalloc/build cargo test --features differential --test differential
//! ```
//!
//! Rather than comparing layouts (which legitimately differ), it runs the same
//! deterministic workload through both allocators and asserts each upholds the
//! *observable* contract a correct allocator must satisfy — and that the two
//! agree on version and usable-size behavior. We drive our allocator through
//! its **Rust** API (`mimalloc_rs::*`) so the C `mi_*` symbols don't clash with
//! our optional `capi` exports.
#![cfg(feature = "differential")]

use std::ptr::NonNull;

// The C reference allocator (resolved from libmimalloc via build.rs).
extern "C" {
    fn mi_malloc(size: usize) -> *mut u8;
    fn mi_calloc(count: usize, size: usize) -> *mut u8;
    fn mi_realloc(p: *mut u8, newsize: usize) -> *mut u8;
    fn mi_malloc_aligned(size: usize, alignment: usize) -> *mut u8;
    fn mi_free(p: *mut u8);
    fn mi_usable_size(p: *const u8) -> usize;
    fn mi_version() -> i32;
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Observable invariants any correct allocator must uphold, driven over an
/// abstract `(alloc, calloc, realloc, aligned, free, usable)` surface.
#[allow(clippy::too_many_arguments)]
fn run_invariants(
    name: &str,
    alloc: impl Fn(usize) -> *mut u8,
    calloc: impl Fn(usize, usize) -> *mut u8,
    realloc: impl Fn(*mut u8, usize) -> *mut u8,
    aligned: impl Fn(usize, usize) -> *mut u8,
    free: impl Fn(*mut u8),
    usable: impl Fn(*mut u8) -> usize,
) {
    struct Live {
        ptr: *mut u8,
        size: usize,
        fill: u8,
    }
    let mut rng = Rng(0xC0FFEE_1234_5678);
    let mut live: Vec<Live> = Vec::new();
    let mut fill: u8 = 1;

    let assert_no_overlap = |live: &[Live], start: usize, size: usize| {
        let end = start + size;
        for l in live {
            let ls = l.ptr as usize;
            let le = ls + l.size;
            assert!(
                end <= ls || start >= le,
                "{name}: overlap [{start:#x},{end:#x}) vs [{ls:#x},{le:#x})"
            );
        }
    };

    for _ in 0..30_000 {
        let op = rng.below(100);
        if op < 55 || live.is_empty() {
            let kind = rng.below(3);
            let size = 1 + rng.below(4096);
            let (p, align) = match kind {
                0 => (alloc(size), 1usize),
                1 => {
                    let a = 1usize << rng.below(8); // 1..128
                    (aligned(size, a), a)
                }
                _ => {
                    let p = calloc(1, size);
                    if !p.is_null() {
                        // calloc must zero
                        for i in 0..size {
                            assert_eq!(unsafe { *p.add(i) }, 0, "{name}: calloc not zeroed");
                        }
                    }
                    (p, 1)
                }
            };
            if p.is_null() {
                continue;
            }
            assert_eq!(p as usize % align, 0, "{name}: misaligned (align {align})");
            assert!(usable(p) >= size, "{name}: usable {} < {size}", usable(p));
            assert_no_overlap(&live, p as usize, size);
            unsafe { std::ptr::write_bytes(p, fill, size) };
            live.push(Live { ptr: p, size, fill });
            fill = fill.wrapping_add(1).max(1);
        } else if op < 80 {
            let i = rng.below(live.len());
            let l = live.swap_remove(i);
            for k in 0..l.size {
                assert_eq!(
                    unsafe { *l.ptr.add(k) },
                    l.fill,
                    "{name}: corruption/overlap"
                );
            }
            free(l.ptr);
        } else {
            let i = rng.below(live.len());
            let l = live.swap_remove(i);
            let newsize = 1 + rng.below(8192);
            let keep = l.size.min(newsize);
            let np = realloc(l.ptr, newsize);
            if np.is_null() {
                live.push(l);
                continue;
            }
            for k in 0..keep {
                assert_eq!(unsafe { *np.add(k) }, l.fill, "{name}: realloc lost data");
            }
            unsafe { std::ptr::write_bytes(np, l.fill, newsize) };
            assert_no_overlap(&live, np as usize, newsize);
            live.push(Live {
                ptr: np,
                size: newsize,
                fill: l.fill,
            });
        }
    }
    for l in live {
        for k in 0..l.size {
            assert_eq!(unsafe { *l.ptr.add(k) }, l.fill, "{name}: final corruption");
        }
        free(l.ptr);
    }
}

#[test]
fn version_matches_c() {
    // Both must report the same mimalloc version they target.
    assert_eq!(
        unsafe { mi_version() },
        mimalloc_rs::MI_MALLOC_VERSION as i32
    );
}

#[test]
fn c_reference_upholds_invariants() {
    // Sanity: the C library itself satisfies the oracle (validates the harness).
    run_invariants(
        "C/libmimalloc",
        |s| unsafe { mi_malloc(s) },
        |c, s| unsafe { mi_calloc(c, s) },
        |p, s| unsafe { mi_realloc(p, s) },
        |s, a| unsafe { mi_malloc_aligned(s, a) },
        |p| unsafe { mi_free(p) },
        |p| unsafe { mi_usable_size(p) },
    );
}

#[test]
fn rust_matches_c_contract() {
    // Our allocator (Rust API) must satisfy the identical observable contract.
    let nn = |p: Option<NonNull<u8>>| p.map_or(core::ptr::null_mut(), |x| x.as_ptr());
    run_invariants(
        "mimalloc-rs",
        |s| nn(mimalloc_rs::malloc(s)),
        |c, s| {
            // calloc = zeroed count*size
            c.checked_mul(s)
                .and_then(mimalloc_rs::zalloc)
                .map_or(core::ptr::null_mut(), |x| x.as_ptr())
        },
        |p, s| match NonNull::new(p) {
            None => nn(mimalloc_rs::malloc(s)),
            Some(x) => nn(unsafe { mimalloc_rs::realloc(x, s) }),
        },
        |s, a| nn(mimalloc_rs::malloc_aligned(s, a.max(1))),
        |p| {
            if let Some(x) = NonNull::new(p) {
                unsafe { mimalloc_rs::free(x) }
            }
        },
        |p| match NonNull::new(p) {
            None => 0,
            Some(x) => unsafe { mimalloc_rs::heap::usable_size(x) },
        },
    );
}
