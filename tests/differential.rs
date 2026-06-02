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
//! deterministic workloads through both allocators and asserts each upholds the
//! *observable* contract a correct allocator must satisfy — and that the two
//! agree on version. Coverage: a randomized single-threaded workload across all
//! size bins (small/medium, large ≥16 KiB, huge ≥512 KiB), a deterministic
//! edge-case API surface (`free(NULL)`, `realloc(NULL/0/grow/shrink across
//! bins)`, `calloc` overflow, large alignments, `usable_size(NULL)`), and a
//! producer/consumer **cross-thread** workload (allocate on one thread, free on
//! another). We drive our allocator through its **Rust** API (`mimalloc_rs::*`)
//! so the C `mi_*` symbols don't clash with our optional `capi` exports.
//!
//! Gated on `have_c_mimalloc` (set by `build.rs` only when `MIMALLOC_C_LIB` is
//! provided): without a real C library the `extern "C" mi_*` block would
//! resolve to our own `capi` exports and silently test the allocator against
//! itself, so the test compiles to nothing unless the C library is linked.
#![cfg(all(feature = "differential", have_c_mimalloc))]

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

/// A size that mostly lands in the small/medium bins but with a low-probability
/// large (≥16 KiB) and huge (≥512 KiB) tail, so every allocation path —
/// small-page, large-page, and the dedicated huge path — is exercised against
/// the C reference, not just the small bins.
fn pick_size(rng: &mut Rng) -> usize {
    match rng.below(100) {
        0 => 512 * 1024 + rng.below(1024 * 1024), // huge path (~1%)
        1..=4 => 16 * 1024 + rng.below(480 * 1024), // large path (~4%)
        _ => 1 + rng.below(4096),                 // small/medium (~95%)
    }
}

/// Write `fill` over a representative sample of the block: the whole block when
/// small (≤4 KiB → full byte coverage, unchanged from before), otherwise the
/// first 4 KiB plus the last byte. This keeps large/huge ops cheap while still
/// catching overlap and content loss at both ends of the block.
fn fill_block(p: *mut u8, size: usize, fill: u8) {
    let head = size.min(4096);
    // SAFETY: `p` is a live allocation of at least `size` bytes.
    unsafe {
        std::ptr::write_bytes(p, fill, head);
        if size > head {
            *p.add(size - 1) = fill;
        }
    }
}

/// Verify the sample written by [`fill_block`] still reads back as `fill`.
fn check_block(name: &str, p: *mut u8, size: usize, fill: u8, ctx: &str) {
    let head = size.min(4096);
    for k in 0..head {
        // SAFETY: `p` is a live allocation of at least `size` bytes.
        assert_eq!(unsafe { *p.add(k) }, fill, "{name}: {ctx} (+{k})");
    }
    if size > head {
        // SAFETY: same.
        assert_eq!(unsafe { *p.add(size - 1) }, fill, "{name}: {ctx} (end)");
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
    let mut rng = Rng(0x00C0_FFEE_1234_5678);
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
            let size = pick_size(&mut rng);
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
            fill_block(p, size, fill);
            live.push(Live { ptr: p, size, fill });
            fill = fill.wrapping_add(1).max(1);
        } else if op < 80 {
            let i = rng.below(live.len());
            let l = live.swap_remove(i);
            check_block(name, l.ptr, l.size, l.fill, "corruption/overlap");
            free(l.ptr);
        } else {
            let i = rng.below(live.len());
            let l = live.swap_remove(i);
            let newsize = pick_size(&mut rng);
            let keep = l.size.min(newsize);
            let np = realloc(l.ptr, newsize);
            if np.is_null() {
                live.push(l);
                continue;
            }
            // realloc must preserve the first `keep` bytes (sampled for big blocks).
            let vk = keep.min(4096);
            for k in 0..vk {
                assert_eq!(
                    unsafe { *np.add(k) },
                    l.fill,
                    "{name}: realloc lost data (+{k})"
                );
            }
            fill_block(np, newsize, l.fill);
            assert_no_overlap(&live, np as usize, newsize);
            live.push(Live {
                ptr: np,
                size: newsize,
                fill: l.fill,
            });
        }
    }
    for l in live {
        check_block(name, l.ptr, l.size, l.fill, "final corruption");
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

/// Deterministic edge-case API contract that both allocators must uphold.
#[allow(clippy::too_many_arguments)]
fn edge_cases(
    name: &str,
    calloc: impl Fn(usize, usize) -> *mut u8,
    realloc: impl Fn(*mut u8, usize) -> *mut u8,
    aligned: impl Fn(usize, usize) -> *mut u8,
    free: impl Fn(*mut u8),
    usable: impl Fn(*mut u8) -> usize,
) {
    // free(NULL) and usable_size(NULL) are well-defined no-ops.
    free(core::ptr::null_mut());
    assert_eq!(
        usable(core::ptr::null_mut()),
        0,
        "{name}: usable(NULL) != 0"
    );

    // realloc(NULL, n) behaves like malloc(n).
    let p = realloc(core::ptr::null_mut(), 64);
    assert!(!p.is_null() && usable(p) >= 64, "{name}: realloc(NULL,64)");
    fill_block(p, 64, 0xA5);

    // realloc growing across a bin boundary (small -> large) preserves content.
    let q = realloc(p, 200 * 1024);
    assert!(
        !q.is_null() && usable(q) >= 200 * 1024,
        "{name}: realloc grow"
    );
    check_block(name, q, 64, 0xA5, "realloc grow lost data");
    fill_block(q, 200 * 1024, 0x5A);
    // ...and shrinking back across the boundary preserves the kept prefix.
    let r = realloc(q, 128);
    assert!(!r.is_null() && usable(r) >= 128, "{name}: realloc shrink");
    check_block(name, r, 128, 0x5A, "realloc shrink lost data");
    free(r);

    // calloc overflow must fail cleanly (null), not wrap.
    assert!(
        calloc(usize::MAX, 2).is_null(),
        "{name}: calloc overflow not null"
    );

    // Large alignment (> page) is honored and the block is usable + zeroed.
    let a = aligned(4096, 64 * 1024);
    assert!(!a.is_null(), "{name}: aligned(4K, 64K) null");
    assert_eq!(a as usize % (64 * 1024), 0, "{name}: 64K alignment");
    assert!(usable(a) >= 4096, "{name}: aligned usable");
    free(a);

    // calloc zeroes even a huge block.
    let z = calloc(1, 600 * 1024);
    if !z.is_null() {
        for k in [0usize, 4096, 300 * 1024, 600 * 1024 - 1] {
            assert_eq!(
                unsafe { *z.add(k) },
                0,
                "{name}: huge calloc not zeroed (+{k})"
            );
        }
        free(z);
    }
}

/// Producer/consumer cross-thread workload (blocks allocated on one thread are
/// freed on another), the hard concurrency case. Each block carries a known
/// fill verified across the hand-off; must complete without corruption or crash.
fn run_cross_thread(
    name: &str,
    alloc: impl Fn(usize) -> *mut u8 + Copy + Send,
    free: impl Fn(*mut u8) + Copy + Send,
) {
    use std::sync::mpsc;
    const PAIRS: usize = 4;
    const PER: usize = 4000;
    std::thread::scope(|scope| {
        for t in 0..PAIRS {
            let (tx, rx) = mpsc::channel::<(usize, usize, u8)>();
            scope.spawn(move || {
                let mut rng = Rng(0xABCD_0000 + t as u64);
                for _ in 0..PER {
                    let size = 1 + rng.below(2048);
                    let p = alloc(size);
                    if p.is_null() {
                        continue;
                    }
                    let fill = (t as u8).wrapping_mul(31) | 1; // non-zero, per-thread
                    fill_block(p, size, fill);
                    if tx.send((p as usize, size, fill)).is_err() {
                        break;
                    }
                }
            });
            scope.spawn(move || {
                while let Ok((addr, size, fill)) = rx.recv() {
                    let p = addr as *mut u8;
                    check_block(name, p, size, fill, "cross-thread corruption");
                    free(p);
                }
            });
        }
    });
}

#[test]
fn edge_cases_c_and_rust() {
    edge_cases(
        "C/libmimalloc",
        |c, s| unsafe { mi_calloc(c, s) },
        |p, s| unsafe { mi_realloc(p, s) },
        |s, a| unsafe { mi_malloc_aligned(s, a) },
        |p| unsafe { mi_free(p) },
        |p| unsafe { mi_usable_size(p) },
    );
    let nn = |p: Option<NonNull<u8>>| p.map_or(core::ptr::null_mut(), |x| x.as_ptr());
    edge_cases(
        "mimalloc-rs",
        |c, s| {
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

#[test]
fn cross_thread_c_upholds_invariants() {
    run_cross_thread(
        "C/libmimalloc",
        |s| unsafe { mi_malloc(s) },
        |p| unsafe { mi_free(p) },
    );
}

#[test]
fn cross_thread_rust_matches_contract() {
    run_cross_thread(
        "mimalloc-rs",
        |s| mimalloc_rs::malloc(s).map_or(core::ptr::null_mut(), |x| x.as_ptr()),
        |p| {
            if let Some(x) = NonNull::new(p) {
                unsafe { mimalloc_rs::free(x) }
            }
        },
    );
}
