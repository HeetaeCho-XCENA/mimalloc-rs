---
name: Bug report
about: A crash, miscompile, UB, leak, or wrong behavior
title: "[bug] "
labels: bug
---

<!-- SPDX-License-Identifier: MIT -->

**What happened**
<!-- Observed behavior: crash/abort message, sanitizer/Miri report, wrong result, leak. -->

**Expected**
<!-- What a correct allocator should have done. -->

**Reproduce**
<!-- Minimal steps/code. A failing test, a fuzz input, or an op sequence is ideal. -->

```rust
// minimal repro, if possible
```

**Environment**
- mimalloc-rs commit / version:
- features (e.g. `secure,debug`, `nightly`, `override`):
- toolchain (`rustc -V`) and OS:
- how detected (tests / Miri / loom / ASan / TSan / differential / fuzz / app):
