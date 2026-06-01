// SPDX-License-Identifier: MIT
//! A **minimal-overhead, single-allocator** alloc/free microbench for isolating
//! the allocator's own cost — built to be measured with `perf stat`.
//!
//! `profile_alloc`/`bench_suite` mix per-iteration noise (xorshift RNG,
//! `Vec::swap_remove`, `Layout` construction, a memory touch) into the loop,
//! which inflates "main" in a profile and muddies how many instructions the
//! *allocator* actually executes. This harness strips all of that: a fixed
//! size, a precomputed `Layout`, and a power-of-two **ring of slots** (`i & mask`
//! — no RNG, no `Vec` growth, no per-iter `Layout`). Each iteration frees the
//! old occupant of a slot and allocates a new block into it: one alloc + one
//! free, with only a mask, an index, a branch, and a store around them.
//!
//! Crucially it runs **one allocator per process** (selected by `MB_ALLOC`), so
//! `perf stat ./microbench` attributes its counters to exactly that allocator
//! plus the (identical across allocators) loop overhead. Compare allocators by
//! running it twice and diffing the counters:
//!
//! ```sh
//! RUSTFLAGS="-C debuginfo=1 -C force-frame-pointers=yes" \
//!   cargo build --release --example microbench
//!
//! MB_ALLOC=rs perf stat -e instructions,cycles,L1-dcache-load-misses -- \
//!   ./target/release/examples/microbench
//! MIMALLOC_C_LIB=/path/to/mimalloc/build MB_ALLOC=c perf stat -e instructions,cycles,L1-dcache-load-misses -- \
//!   ./target/release/examples/microbench
//! ```
//! The loop overhead is identical for both, so the **difference** in
//! instructions ÷ (2 × `MB_ROUNDS`) is the per-op (alloc+free) allocator
//! instruction delta. It answers the open question:
//!   - rs runs **many more instructions** ⇒ we do more work → a leaner fast path
//!     could close the gap (code win).
//!   - rs runs **similar instructions but more cycles/misses** ⇒ microarchitectural
//!     (cache/TLS/port stalls) → little to gain from shaving instructions.
//!
//! Caveat: the C reference is called through an indirect `dlopen` pointer, so a
//! few FFI-thunk instructions are charged to `c` — this biases *against* "rs has
//! more instructions", so if rs still shows more, the signal is real.
//!
//! Tunables: `MB_ALLOC` (rs|c|system, default rs), `MB_ROUNDS` (default 5e7),
//! `MB_N` (ring size, rounded up to a power of two, default 2048),
//! `MB_SIZE` (fixed block size, default 64), `MB_TOUCH` (1 = write one byte;
//! default 0 = isolate the allocator from page-fault/cache cost),
//! `MB_PIN_CORE` (default 2).

use mimalloc_rs::MiMalloc;
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;

#[derive(Clone, Copy)]
struct CMiMalloc {
    malloc_aligned: unsafe extern "C" fn(usize, usize) -> *mut u8,
    free: unsafe extern "C" fn(*mut u8),
}
// SAFETY: the canonical C `mi_*` entry points resolved from libmimalloc; they
// honor size+alignment (or return null) and free their own blocks.
unsafe impl GlobalAlloc for CMiMalloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: FFI call with size>=1 and a power-of-two alignment.
        unsafe { (self.malloc_aligned)(layout.size().max(1), layout.align()) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: `ptr` came from `self.malloc_aligned`.
        unsafe { (self.free)(ptr) }
    }
}

#[cfg(unix)]
fn load_c_mimalloc() -> Option<CMiMalloc> {
    let dir = std::env::var("MIMALLOC_C_LIB").ok()?;
    if dir.is_empty() {
        return None;
    }
    let path = std::ffi::CString::new(format!("{dir}/libmimalloc.so")).ok()?;
    // SAFETY: standard dlopen/dlsym; symbols transmuted only after null checks.
    unsafe {
        let h = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if h.is_null() {
            return None;
        }
        let m = libc::dlsym(h, c"mi_malloc_aligned".as_ptr());
        let f = libc::dlsym(h, c"mi_free".as_ptr());
        if m.is_null() || f.is_null() {
            return None;
        }
        Some(CMiMalloc {
            malloc_aligned: core::mem::transmute::<
                *mut core::ffi::c_void,
                unsafe extern "C" fn(usize, usize) -> *mut u8,
            >(m),
            free: core::mem::transmute::<*mut core::ffi::c_void, unsafe extern "C" fn(*mut u8)>(f),
        })
    }
}
#[cfg(not(unix))]
fn load_c_mimalloc() -> Option<CMiMalloc> {
    None
}

#[cfg(target_os = "linux")]
fn pin_to_core(core: usize) -> bool {
    // SAFETY: zeroed cpu_set_t is valid; CPU_SET sets one in-range bit.
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}
#[cfg(not(target_os = "linux"))]
fn pin_to_core(_core: usize) -> bool {
    false
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Tight ring roundtrip: free a slot's old occupant, allocate a new block into
/// it. Identical instruction shape for every allocator; `mask`-indexed, no RNG.
#[inline(never)]
fn roundtrip<A: GlobalAlloc>(
    a: &A,
    rounds: usize,
    n_pow2: usize,
    layout: Layout,
    touch: bool,
) -> u64 {
    let mask = n_pow2 - 1;
    let mut slots = vec![core::ptr::null_mut::<u8>(); n_pow2];
    let mut cs = 0u64;
    for i in 0..rounds {
        let s = i & mask;
        let old = slots[s];
        // SAFETY: `old` is null or a block we allocated with `layout`.
        if !old.is_null() {
            unsafe { a.dealloc(old, layout) };
        }
        // SAFETY: standard alloc with a valid layout.
        let p = unsafe { a.alloc(layout) };
        if touch && !p.is_null() {
            // SAFETY: `p` is valid for `layout.size() >= 1` bytes.
            unsafe { *p = 1 };
        }
        slots[s] = p;
        cs = cs.wrapping_add(p as u64);
    }
    for &p in &slots {
        // SAFETY: each non-null slot holds a block allocated with `layout`.
        if !p.is_null() {
            unsafe { a.dealloc(p, layout) };
        }
    }
    cs
}

fn main() {
    let which = std::env::var("MB_ALLOC").unwrap_or_else(|_| "rs".to_string());
    let rounds = env_usize("MB_ROUNDS", 50_000_000);
    let n = env_usize("MB_N", 2048).next_power_of_two();
    let size = env_usize("MB_SIZE", 64);
    let touch = env_usize("MB_TOUCH", 0) != 0;
    let pinned = pin_to_core(env_usize("MB_PIN_CORE", 2));
    let layout = Layout::from_size_align(size, 16).unwrap();

    let t = std::time::Instant::now();
    let cs = match which.as_str() {
        "system" => roundtrip(&System, rounds, n, layout, touch),
        "c" => match load_c_mimalloc() {
            Some(c) => roundtrip(&c, rounds, n, layout, touch),
            None => {
                eprintln!("MB_ALLOC=c but libmimalloc not loaded (set MIMALLOC_C_LIB); aborting");
                std::process::exit(2);
            }
        },
        _ => roundtrip(&MiMalloc, rounds, n, layout, touch),
    };
    let dt = t.elapsed();
    black_box(cs);

    let ops = 2 * rounds; // one alloc + one free per round
    let mops = ops as f64 / dt.as_secs_f64() / 1e6;
    eprintln!(
        "microbench[{which}]: {rounds} rounds (×2 = {ops} ops), N={n}, size={size}, \
         touch={touch}, pinned={pinned} => {dt:.2?} ({mops:.1} Mops/s)"
    );
}
