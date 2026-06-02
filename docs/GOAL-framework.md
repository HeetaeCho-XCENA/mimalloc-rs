<!-- SPDX-License-Identifier: MIT -->
# GOAL — Best-practice verification + benchmark framework (contribution-ready)

> Round brief for `omc ultragoal`. Put the **scaffolding** in place so the
> project meets the bar an open-source allocator is expected to clear and so
> outside contributors can verify + benchmark changes reproducibly. These are
> tooling/CI/docs frames (mostly **non-runtime** — they must not change the
> allocator's behavior), each independently mergeable after CI.

## Why
We already have: `bench_suite`/`microbench`/`profile_alloc`/`rss_spike` examples,
`scripts/perf_compare.sh` (branch-vs-main no-regression harness), and CI for
test/nightly/lint/miri/loom/preload/**differential**. Missing for a serious,
contributable allocator: statistical micro-benchmarks, fuzzing, thread/address
sanitizers, the standard cross-allocator benchmark (mimalloc-bench), coverage,
and the contributor-facing docs/templates that make all of it reproducible.

## Principles / bar
- Frames are **non-runtime**: no change to `src/` allocator behavior. The full
  existing gate (tests, miri, loom, differential, abort gate) must stay green.
- Idiomatic, simple, documented; `// SAFETY:` on any new unsafe (fuzz target).
- CI carries **correctness** only; **perf is never gated in CI** (shared runners
  are too noisy) — perf reproducibility is documented around the pinned-machine
  `perf_compare.sh` protocol and mimalloc-bench.
- Heavy/external tools (mimalloc-bench, sanitizers, cargo-fuzz) must degrade
  cleanly (skip/instructions) when their toolchain/inputs are absent.

## Stories

### FW1 — Criterion statistical micro-benchmarks (`benches/`)
- Add `criterion` dev-dep and a `benches/alloc.rs` (`harness = false`) measuring
  alloc+free across the size classes (small/medium/large/huge) and a couple of
  realistic patterns (fixed-size ring, mixed sizes), optionally a multi-thread
  group. Statistical (mean/CI/outlier) results with regression deltas across runs
  (`cargo bench -- --baseline`). Pure dev — no `src/` change.
- **DoD**: `cargo bench` runs and reports; documented; default tests unaffected;
  fmt/clippy clean (benches included where lintable).

### FW2 — Fuzzing harness (`fuzz/`, cargo-fuzz + ASan)
- A `fuzz/` cargo-fuzz crate with a target driving a randomized
  alloc/free/realloc/aligned op stream decoded from the fuzz input, asserting the
  observable invariants (reuse the differential oracle: alignment, usable_size,
  no-overlap of live blocks, realloc preservation, no double-free/leak), run under
  AddressSanitizer. Deterministic decode so crashes reproduce.
- **DoD**: `cargo +nightly fuzz run alloc -- -runs=N` builds and runs a smoke
  batch clean locally; a CI job runs a short bounded smoke (`-runs`/`-max_total_time`)
  so regressions surface without an unbounded job; documented seed-corpus layout.

### FW3 — Sanitizer + coverage CI
- CI jobs (nightly): **ASan** and **ThreadSanitizer** over the lib tests + the
  cross-thread stress (`-Zsanitizer=address|thread`, explicit target). TSan is
  the key complement to loom — it catches races at real scale. Add a **coverage**
  job (`cargo-llvm-cov`) emitting lcov (codecov optional).
- **DoD**: ASan + TSan jobs green (no leak/race), coverage job produces a report;
  jobs are bounded and skip cleanly if a sanitizer is unavailable.

### FW4 — mimalloc-bench runner (`scripts/mimalloc-bench.sh`)
- Script that builds mimalloc-rs as a cdylib (`override` feature), clones/builds
  `daanx/mimalloc-bench`, and runs its standard workloads (cfrac, espresso,
  larson, mstress, rptest, xmalloc-test, …) via `LD_PRELOAD`, comparing
  mimalloc-rs vs C `libmimalloc` vs system. Prints a per-workload table.
- **DoD**: script builds the cdylib and is structured to run the suite; documented
  prerequisites; **not** wired into CI (heavy + perf-noisy) — it's a local/pinned
  tool. Degrades cleanly if mimalloc-bench isn't present.

### FW5 — Contribution scaffolding
- `CONTRIBUTING.md`: dev setup, MSRV, the full gate checklist (test / `--features
  secure,debug,stats,track` / miri / loom / differential / fuzz / sanitizers),
  the **no-regression perf protocol** (pinned-machine `perf_compare.sh` +
  mimalloc-bench), the documented-`unsafe` policy, PR expectations.
- `docs/benchmarking.md` + `docs/verification.md`: how to reproduce every bench
  and every verification gate (consolidating the scattered recipes).
- `.github/` PR + issue templates, `CHANGELOG.md`, `CODE_OF_CONDUCT.md`.
- **DoD**: docs accurate against the actual commands; links resolve; no broken
  references.

## Verification (round)
Each story: `cargo build`/`cargo test` unaffected, fmt + clippy clean, its own CI
job green (FW2/FW3), and the existing gates (miri/loom/differential/abort) stay
green. No `src/` behavior change ⇒ no pinned-machine perf gate. Final gate:
ai-slop-cleaner + verifier + code-review on the round's diff.
