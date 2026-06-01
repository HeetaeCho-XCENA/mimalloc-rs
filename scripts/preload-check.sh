#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# End-to-end LD_PRELOAD verification for mimalloc-rs.
#
# Builds the override cdylib, compiles an unmodified C probe program, and proves
# that — under LD_PRELOAD — every libc `malloc` is served by our allocator
# (`mi_is_in_heap_region(p) == true`). Also smoke-tests a real system binary
# under preload to confirm transparent replacement doesn't crash it.
#
# The script's exit status is the source of truth: non-zero == FAIL.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# --- 0. Require a C compiler; otherwise this is a clean no-op (not a failure) --
CC="${CC:-cc}"
if ! command -v "$CC" >/dev/null 2>&1; then
    echo "SKIP: no C compiler (\$CC='$CC' not found); preload check not run."
    exit 0
fi

# --- temp workspace + cleanup ------------------------------------------------
WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

# --- 1/2. Build the preload cdylib into an isolated target dir ---------------
# Isolated --target-dir so this never clobbers/locks the normal build (and so a
# nested `cargo test` invocation can't deadlock on the same target lock).
#
# NOTE: CI sets a global `RUSTFLAGS: -D warnings`. We must combine our cfg with
# it, because setting RUSTFLAGS here fully *replaces* the inherited value.
PRELOAD_TARGET="$REPO_ROOT/target/preload"
echo "Building override cdylib (RUSTFLAGS='--cfg override_export -D warnings')..."
RUSTFLAGS="--cfg override_export -D warnings" \
    cargo rustc --release --features override --crate-type cdylib \
    --target-dir "$PRELOAD_TARGET"

SO="$PRELOAD_TARGET/release/libmimalloc_rs.so"
if [[ ! -f "$SO" ]]; then
    echo "FAIL: expected cdylib not found at $SO"
    exit 1
fi
echo "cdylib: $SO"

# --- 3. (compiler already checked above) -------------------------------------

# --- 4. Write + compile the C probe ------------------------------------------
PROBE_C="$WORK/probe.c"
PROBE_BIN="$WORK/probe"
cat >"$PROBE_C" <<'EOF'
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
// Resolved from the preloaded lib at runtime; weak so the program also links
// and runs (degrading the check) when NOT preloaded.
extern int mi_is_in_heap_region(const void*) __attribute__((weak));
int main(void) {
    int total = 0, ours = 0;
    const int has_probe = (&mi_is_in_heap_region != 0);
    for (int i = 0; i < 2000; i++) {
        size_t n = 16 + (size_t)(i % 8192);
        void* p = malloc(n);
        if (!p) return 2;
        memset(p, 0xAB, n);
        total++;
        if (has_probe && mi_is_in_heap_region(p)) ours++;
        void* q = realloc(p, n * 2 + 1);
        if (!q) return 3;
        free(q);
    }
    void* c = calloc(64, 64);
    if (!c) return 4;
    for (int i = 0; i < 64*64; i++) if (((char*)c)[i] != 0) return 5; // calloc zeroes
    free(c);
    char* s = strdup("mimalloc-rs preload");
    if (!s || strcmp(s, "mimalloc-rs preload") != 0) return 6;
    free(s);
    void* a = NULL;
    if (posix_memalign(&a, 256, 1000) != 0 || ((size_t)a % 256) != 0) return 7;
    free(a);
    printf("ours=%d total=%d probe=%d\n", ours, total, has_probe);
    return 0;
}
EOF

echo "Compiling probe with '$CC -O2'..."
"$CC" -O2 -o "$PROBE_BIN" "$PROBE_C"

# --- 6. (optional) run WITHOUT preload to show it still runs -----------------
# Degrades gracefully: reports ours=0 probe=0. Not a failure condition.
echo "--- probe WITHOUT preload (informational) ---"
if no_out="$("$PROBE_BIN")"; then
    echo "no-preload: $no_out"
else
    echo "no-preload: probe exited non-zero (informational only)"
fi

# --- 5. run WITH preload and assert ours == total and probe == 1 -------------
echo "--- probe WITH preload (authoritative) ---"
if ! out="$(LD_PRELOAD="$SO" "$PROBE_BIN")"; then
    echo "FAIL: probe exited non-zero under LD_PRELOAD"
    exit 1
fi
echo "preload: $out"

# Robust parse via awk: extract each key=value.
ours="$(printf '%s\n' "$out"  | awk -F'[= ]' '{for(i=1;i<NF;i++) if($i=="ours")  print $(i+1)}')"
total="$(printf '%s\n' "$out" | awk -F'[= ]' '{for(i=1;i<NF;i++) if($i=="total") print $(i+1)}')"
probe="$(printf '%s\n' "$out" | awk -F'[= ]' '{for(i=1;i<NF;i++) if($i=="probe") print $(i+1)}')"

if [[ -z "$ours" || -z "$total" || -z "$probe" ]]; then
    echo "FAIL: could not parse probe output: '$out'"
    exit 1
fi
if [[ "$probe" != "1" ]]; then
    echo "FAIL: probe symbol (mi_is_in_heap_region) was not resolved from the preloaded lib (probe=$probe)"
    exit 1
fi
if [[ "$ours" != "$total" ]]; then
    echo "FAIL: only $ours of $total mallocs were served by mimalloc-rs (expected all)"
    exit 1
fi
echo "OK: all $total mallocs served by mimalloc-rs (ours==total, probe=1)"

# --- 7. stress a real system binary under preload ----------------------------
echo "--- transparent-replacement smoke test ---"
if [[ -x /bin/ls ]]; then
    # Use the temp workspace (guaranteed readable — we created it) so an
    # unreadable subdir elsewhere can't turn a benign `ls` error into a false
    # FAIL. The point is only that a real, unmodified binary runs under preload.
    if LD_PRELOAD="$SO" /bin/ls -laR "$WORK" >/dev/null 2>&1; then
        echo "OK: /bin/ls -laR ran under preload"
    else
        echo "FAIL: /bin/ls crashed/failed under LD_PRELOAD"
        exit 1
    fi
else
    # Dependency-free fallback.
    if LD_PRELOAD="$SO" bash -c 'for i in $(seq 1 1000); do :; done'; then
        echo "OK: bash loop ran under preload"
    else
        echo "FAIL: bash loop failed under LD_PRELOAD"
        exit 1
    fi
fi

# --- 8. done -----------------------------------------------------------------
echo "PRELOAD CHECK: PASS"
