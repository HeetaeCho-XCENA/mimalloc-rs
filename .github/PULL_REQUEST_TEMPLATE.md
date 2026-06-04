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
- [ ] **perf**: no regression (pinned machine) — *required for perf-affecting changes*; build `examples/stress` on this branch and `main`, interleave, paste medians:

```
<stress branch-vs-main medians, or "N/A — no perf-affecting change">
```

## Notes / caveats

<!-- New unsafe (SAFETY rationale), follow-ups, anything reviewers should know. -->
