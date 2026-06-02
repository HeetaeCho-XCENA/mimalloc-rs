<!-- SPDX-License-Identifier: MIT -->
## What

<!-- The change, in one or two sentences. If it ports a v3 mechanism, cite the
     C source (e.g. `src/arena.c:…`). -->

## Why

<!-- Motivation / the problem it solves. -->

## Verification

<!-- Tick what you ran (see CONTRIBUTING.md / docs/verification.md). -->

- [ ] `cargo test` + `cargo test --features secure,debug,stats,track`
- [ ] `cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings`
- [ ] loom (`RUSTFLAGS="--cfg loom" cargo test --lib loom_tests`) — if concurrency touched
- [ ] Miri (`cargo +nightly miri test --lib …`) — if pure-logic/provenance touched
- [ ] differential (`MIMALLOC_C_LIB=<dir> cargo test --features differential --test differential`)
- [ ] secure+debug abort gate clean
- [ ] **perf**: no regression on any `bench_suite` phase (pinned machine) — *required for perf-affecting changes*; paste `scripts/perf_compare.sh` output:

```
<perf_compare.sh output, or "N/A — no perf-affecting change">
```

## Notes / caveats

<!-- New unsafe (SAFETY rationale), follow-ups, anything reviewers should know. -->
