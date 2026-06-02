<!-- SPDX-License-Identifier: MIT -->
# Verification

mimalloc-rs is verified by layered, complementary techniques — the standard
stack for a serious allocator. Every gate below also runs in CI
(`.github/workflows/ci.yml`), except where noted.

| Gate | What it catches | Command |
|---|---|---|
| Unit / integration tests | functional correctness | `cargo test` |
| Hardened build | double/invalid-free, padding/canary, stats | `cargo test --features secure,debug,stats,track` |
| MSRV | 1.84 compatibility | `cargo +1.84.0 test` |
| Lint / format | style, common bugs | `cargo clippy --all-targets -- -D warnings`; `cargo fmt --all -- --check` |
| **Miri** (strict-provenance) | UB, provenance violations on the pure-logic modules | `cargo +nightly miri test --lib bitmap` (also `free_list`, `layout`, `bits`) |
| **loom** | exhaustive model-checking of the concurrent CAS protocols (bitmap, xthread_free MPSC, purge claim) | `RUSTFLAGS="--cfg loom" cargo test --lib loom_tests` |
| **Differential** vs C | observable-contract equivalence to mimalloc v3 | `MIMALLOC_C_LIB=<dir> cargo test --features differential --test differential` |
| **Fuzzing** (ASan) | OOB/UAF/leak over randomized op streams | `cargo +nightly fuzz run alloc` (bounded in CI) |
| **AddressSanitizer** | OOB/UAF/leak in unsafe paths | `RUSTFLAGS="-Zsanitizer=address" cargo +nightly test --lib --target x86_64-unknown-linux-gnu` |
| **ThreadSanitizer** | data races at real scale (complements loom) | `RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu --lib -- <cross-thread tests>` |
| Coverage | exercised-code visibility | `cargo llvm-cov --lib` |
| LD_PRELOAD | transparent system-allocator override works | `bash scripts/preload-check.sh` |

## Notes

- **Miri scope**: only the pure-logic modules (`bitmap`/`free_list`/`layout`/
  `bits`) run under Miri — the OS-backed paths use real `mmap` and can't.
- **TSan** needs `-Zbuild-std` (and the `rust-src` component) for accurate
  instrumentation; in CI it runs over the four cross-thread stress tests
  (`stress_owner_retire_vs_cross_thread_free`, `cross_thread_free_collected_by_owner`,
  `collect_reclaims_cross_thread_frees`, `abandoned_pages_reclaimed_across_threads`).
- **Differential** is gated on `have_c_mimalloc` (set by `build.rs` only when
  `MIMALLOC_C_LIB` is provided) so it can never silently test rs against itself;
  CI builds the matching C `libmimalloc` (v3.3.2) to run it.
- The **abort gate**: run the hardened suite repeatedly in parallel to surface
  the secure/debug double-/invalid-free detection:
  `for i in $(seq 1 15); do cargo test --features capi,secure,debug,stats,track --lib; done`.
- Performance is **not** a verification gate here — see `docs/benchmarking.md`
  and the no-regression rule in `CONTRIBUTING.md`.
