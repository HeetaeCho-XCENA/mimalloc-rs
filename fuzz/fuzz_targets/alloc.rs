// SPDX-License-Identifier: MIT
//! Fuzz target: drive a deterministic alloc/free/realloc/aligned op stream
//! decoded from the fuzz input through mimalloc-rs, asserting the observable
//! contract a correct allocator must uphold. Run under AddressSanitizer
//! (cargo-fuzz default), so any out-of-bounds / use-after-free / leak surfaces.
//!
//!   cargo +nightly fuzz run alloc                 # fuzz indefinitely
//!   cargo +nightly fuzz run alloc -- -runs=200000 # bounded smoke (CI)
//!
//! Same oracle as `tests/differential.rs`, but fuzzer-driven: alignment,
//! `usable_size >= size`, no overlap of live blocks, realloc preservation,
//! and no double-free / leak (all blocks freed before return).
#![no_main]

use libfuzzer_sys::fuzz_target;
use mimalloc_rs::{free, malloc, malloc_aligned, realloc};
use std::ptr::NonNull;

struct Live {
    ptr: NonNull<u8>,
    size: usize,
    fill: u8,
}

fn no_overlap(live: &[Live], start: usize, size: usize) {
    let end = start + size;
    for l in live {
        let ls = l.ptr.as_ptr() as usize;
        let le = ls + l.size;
        assert!(
            end <= ls || start >= le,
            "overlap [{start:#x},{end:#x}) vs [{ls:#x},{le:#x})"
        );
    }
}

fn usable(p: NonNull<u8>) -> usize {
    // SAFETY: `p` is a live allocation from this allocator.
    unsafe { mimalloc_rs::heap::usable_size(p) }
}

fn fill_block(p: NonNull<u8>, size: usize, fill: u8) {
    // SAFETY: `p` is writable for `size` bytes.
    unsafe { core::ptr::write_bytes(p.as_ptr(), fill, size) };
}

fn check_block(p: NonNull<u8>, size: usize, fill: u8, ctx: &str) {
    for k in 0..size {
        // SAFETY: `p` is readable for `size` bytes.
        assert_eq!(unsafe { *p.as_ptr().add(k) }, fill, "{ctx} (+{k})");
    }
}

fuzz_target!(|data: &[u8]| {
    let mut live: Vec<Live> = Vec::new();
    let mut fill: u8 = 1;
    let mut next_fill = || {
        let f = fill;
        fill = fill.wrapping_add(1).max(1);
        f
    };
    // Bound total live memory so a pathological input can't OOM the fuzzer.
    const MAX_LIVE_BYTES: usize = 64 * 1024 * 1024;
    let mut live_bytes: usize = 0;

    let mut i = 0usize;
    // Each op consumes: 1 opcode byte + 2 param bytes.
    while i + 2 < data.len() {
        let op = data[i];
        let raw = ((data[i + 1] as usize) << 8) | data[i + 2] as usize; // 0..=65535
        i += 3;

        match op % 4 {
            // malloc
            0 => {
                let size = 1 + raw % 8192;
                if live_bytes + size > MAX_LIVE_BYTES {
                    continue;
                }
                if let Some(p) = malloc(size) {
                    assert!(usable(p) >= size, "usable {} < {size}", usable(p));
                    no_overlap(&live, p.as_ptr() as usize, size);
                    let f = next_fill();
                    fill_block(p, size, f);
                    live.push(Live {
                        ptr: p,
                        size,
                        fill: f,
                    });
                    live_bytes += size;
                }
            }
            // malloc_aligned
            1 => {
                let align = 1usize << (data[i - 3] as usize % 16); // 1..=32768
                let size = 1 + raw % 4096;
                if live_bytes + size > MAX_LIVE_BYTES {
                    continue;
                }
                if let Some(p) = malloc_aligned(size, align) {
                    assert_eq!(p.as_ptr() as usize % align, 0, "misaligned (align {align})");
                    assert!(usable(p) >= size);
                    no_overlap(&live, p.as_ptr() as usize, size);
                    let f = next_fill();
                    fill_block(p, size, f);
                    live.push(Live {
                        ptr: p,
                        size,
                        fill: f,
                    });
                    live_bytes += size;
                }
            }
            // free one
            2 => {
                if !live.is_empty() {
                    let l = live.swap_remove(raw % live.len());
                    check_block(l.ptr, l.size, l.fill, "corruption before free");
                    live_bytes -= l.size;
                    // SAFETY: `l.ptr` is a live block freed exactly once.
                    unsafe { free(l.ptr) };
                }
            }
            // realloc one
            _ => {
                if !live.is_empty() {
                    let l = live.swap_remove(raw % live.len());
                    let newsize = 1 + (raw / 2) % 8192;
                    let keep = l.size.min(newsize);
                    live_bytes -= l.size;
                    // SAFETY: `l.ptr` is a live block.
                    match unsafe { realloc(l.ptr, newsize) } {
                        Some(np) => {
                            check_block(np, keep, l.fill, "realloc lost data");
                            assert!(usable(np) >= newsize);
                            no_overlap(&live, np.as_ptr() as usize, newsize);
                            fill_block(np, newsize, l.fill);
                            live.push(Live {
                                ptr: np,
                                size: newsize,
                                fill: l.fill,
                            });
                            live_bytes += newsize;
                        }
                        None => {
                            // realloc failed: the original is still valid.
                            live.push(l);
                            live_bytes += live.last().unwrap().size;
                        }
                    }
                }
            }
        }
    }

    // No leak: every live block is freed before return.
    for l in live {
        check_block(l.ptr, l.size, l.fill, "final corruption");
        // SAFETY: live block, freed once.
        unsafe { free(l.ptr) };
    }
});
