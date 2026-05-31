// SPDX-License-Identifier: MIT
//! Differential-by-invariant verification.
//!
//! Rather than diffing against a reference allocator bit-for-bit (layouts
//! legitimately differ), we run a long, deterministic, pseudo-random workload
//! of `alloc`/`alloc_aligned`/`zalloc`/`realloc`/`free` and assert the
//! *observable* invariants any correct allocator must satisfy — exactly the
//! oracle the C differential harness compares (see `tests/README` notes):
//!
//! * returned pointers honor the requested alignment,
//! * `zalloc` memory is zeroed,
//! * usable size ≥ requested size,
//! * **no two live allocations overlap**,
//! * `realloc` preserves contents up to `min(old, new)`,
//! * every allocation is freeable (no leak / no crash).

use mimalloc_rs::{free, heap, malloc, malloc_aligned, realloc, zalloc};
use std::ptr::NonNull;

/// Deterministic xorshift64 PRNG (no external deps, reproducible).
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

struct Live {
    ptr: NonNull<u8>,
    size: usize,
    fill: u8,
}

/// Verify the new interval is disjoint from all live allocations.
fn assert_no_overlap(live: &[Live], start: usize, size: usize) {
    let end = start + size;
    for l in live {
        let ls = l.ptr.addr().get();
        let le = ls + l.size;
        assert!(
            end <= ls || start >= le,
            "overlap: new [{start:#x},{end:#x}) vs live [{ls:#x},{le:#x})"
        );
    }
}

#[test]
fn randomized_invariants_single_thread() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut live: Vec<Live> = Vec::new();
    let mut fill_ctr: u8 = 1;

    for _ in 0..40_000usize {
        let op = rng.below(100);
        if op < 55 || live.is_empty() {
            // allocate (occasionally a large/huge object)
            let big = rng.below(20) == 0;
            let size = 1 + rng.below(if big { 200_000 } else { 4096 });
            let kind = rng.below(3);
            let (ptr, align) = match kind {
                0 => (malloc(size), 1usize),
                1 => {
                    // power-of-two alignment up to 256
                    let align = 1usize << rng.below(9); // 1..256
                    (malloc_aligned(size, align), align)
                }
                _ => {
                    let p = zalloc(size);
                    // zalloc must be zeroed
                    if let Some(p) = p {
                        // SAFETY: p valid for `size`.
                        unsafe {
                            for i in 0..size {
                                assert_eq!(*p.as_ptr().add(i), 0, "zalloc not zeroed");
                            }
                        }
                    }
                    (p, 1)
                }
            };
            let Some(ptr) = ptr else { continue };
            // alignment honored
            assert_eq!(ptr.addr().get() % align, 0, "misaligned (align {align})");
            // usable size sufficient
            // SAFETY: live allocation.
            let us = unsafe { heap::usable_size(ptr) };
            assert!(us >= size, "usable {us} < requested {size}");
            // no overlap with other live allocations
            assert_no_overlap(&live, ptr.addr().get(), size);
            // fill with a recognizable byte
            let fill = fill_ctr;
            fill_ctr = fill_ctr.wrapping_add(1).max(1);
            // SAFETY: writing `size` bytes into our block.
            unsafe { std::ptr::write_bytes(ptr.as_ptr(), fill, size) };
            live.push(Live { ptr, size, fill });
        } else if op < 80 {
            // free a random live allocation (after verifying its bytes survived)
            let i = rng.below(live.len());
            let l = live.swap_remove(i);
            // SAFETY: still-live block.
            unsafe {
                for k in 0..l.size {
                    assert_eq!(
                        *l.ptr.as_ptr().add(k),
                        l.fill,
                        "content corrupted (overlap?)"
                    );
                }
                free(l.ptr);
            }
        } else {
            // realloc a random live allocation, preserving content
            let i = rng.below(live.len());
            let l = live.swap_remove(i);
            let new_size = 1 + rng.below(8192);
            let keep = l.size.min(new_size);
            // SAFETY: live block; realloc preserves up to min(old,new).
            let np = unsafe { realloc(l.ptr, new_size) };
            let Some(np) = np else {
                // OOM: put it back (still valid)
                live.push(l);
                continue;
            };
            // SAFETY: np valid for new_size.
            unsafe {
                for k in 0..keep {
                    assert_eq!(*np.as_ptr().add(k), l.fill, "realloc lost content");
                }
                // refill the (possibly grown) block and record new size
                std::ptr::write_bytes(np.as_ptr(), l.fill, new_size);
            }
            assert_no_overlap(&live, np.addr().get(), new_size);
            live.push(Live {
                ptr: np,
                size: new_size,
                fill: l.fill,
            });
        }
    }

    // Drain: every remaining allocation must be freeable with intact content.
    for l in live.drain(..) {
        // SAFETY: live block.
        unsafe {
            for k in 0..l.size {
                assert_eq!(
                    *l.ptr.as_ptr().add(k),
                    l.fill,
                    "content corrupted before final free"
                );
            }
            free(l.ptr);
        }
    }
}
