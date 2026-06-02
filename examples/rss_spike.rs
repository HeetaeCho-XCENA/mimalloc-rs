// SPDX-License-Identifier: MIT
//! RSS spike bench: prove the delayed purge returns freed memory to the OS.
//!
//! Allocates a large working set with mimalloc-rs, touches every page so it
//! becomes resident, frees it all, then forces a `collect` (which force-purges
//! the freed arena slices via `madvise`). It samples the resident set size
//! (RSS, from `/proc/self/statm`) at each phase.
//!
//! With purging enabled (the default) the post-collect RSS drops back toward
//! the baseline. Run the control with purging disabled to see RSS stay near the
//! peak:
//! ```sh
//! cargo run --release --example rss_spike                 # purge on  → RSS drops
//! MIMALLOC_PURGE_DELAY=-1 cargo run --release --example rss_spike  # off → RSS stays
//! ```
//! Tunables: `RSS_BLOCK` (bytes/alloc, default 1 MiB → the huge path, one region
//! per alloc), `RSS_COUNT` (default 128).

use mimalloc_rs::init::{collect, free, malloc};
use std::ptr::NonNull;

#[cfg(target_os = "linux")]
fn rss_bytes() -> usize {
    // statm field 2 is the resident page count.
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: usize = s
        .split_whitespace()
        .nth(1)
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    pages * 4096
}
#[cfg(not(target_os = "linux"))]
fn rss_bytes() -> usize {
    0
}

fn mb(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let block = env_usize("RSS_BLOCK", 1024 * 1024);
    let count = env_usize("RSS_COUNT", 128);
    let purge_off = std::env::var("MIMALLOC_PURGE_DELAY").ok().as_deref() == Some("-1");

    let base = rss_bytes();

    // Allocate and touch every page so the working set is genuinely resident.
    let mut ptrs: Vec<NonNull<u8>> = Vec::with_capacity(count);
    for _ in 0..count {
        let p = malloc(block).expect("allocation failed");
        // SAFETY: `block` bytes are writable for a fresh allocation.
        unsafe {
            let mut off = 0;
            while off < block {
                *p.as_ptr().add(off) = 1;
                off += 4096;
            }
        }
        ptrs.push(p);
    }
    let peak = rss_bytes();

    // Free everything (schedules the freed slices for purge).
    for p in ptrs.drain(..) {
        // SAFETY: each pointer came from `malloc` above and is freed once.
        unsafe { free(p) };
    }
    let after_free = rss_bytes();

    // Force a collect: this force-purges the freed slices back to the OS.
    collect(true);
    let after_collect = rss_bytes();

    let spike = peak.saturating_sub(base).max(1);
    let reclaimed = peak.saturating_sub(after_collect);
    eprintln!(
        "rss_spike: block={} count={} working_set={:.0} MiB  purge={}",
        block,
        count,
        mb(block * count),
        if purge_off { "DISABLED" } else { "on" }
    );
    eprintln!("  baseline        {:8.1} MiB", mb(base));
    eprintln!(
        "  peak (touched)  {:8.1} MiB  (+{:.1})",
        mb(peak),
        mb(spike)
    );
    eprintln!("  after free      {:8.1} MiB", mb(after_free));
    eprintln!("  after collect   {:8.1} MiB", mb(after_collect));
    eprintln!(
        "  reclaimed by purge: {:.1} MiB ({:.0}% of the spike)",
        mb(reclaimed),
        100.0 * reclaimed as f64 / spike as f64
    );
}
