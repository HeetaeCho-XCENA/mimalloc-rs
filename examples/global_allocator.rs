// SPDX-License-Identifier: MIT
//! Use mimalloc-rs as the program-wide allocator.
//!
//! Run with: `cargo run --example global_allocator`

use mimalloc_rs::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    // Every standard allocation now goes through mimalloc-rs.
    let mut v: Vec<u64> = Vec::new();
    for i in 0..1_000_000 {
        v.push(i);
    }
    let sum: u64 = v.iter().sum();
    println!("sum of 0..1_000_000 = {sum}");

    let s = String::from("allocated by mimalloc-rs");
    let boxed: Box<[u8]> = vec![0u8; 4096].into_boxed_slice();
    println!("{s}, boxed {} bytes", boxed.len());

    // Nested / many small allocations
    let nested: Vec<String> = (0..10_000).map(|i| format!("item-{i}")).collect();
    println!("created {} strings", nested.len());
}
