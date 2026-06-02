#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Run the standard cross-allocator benchmark suite (daanx/mimalloc-bench) against
# mimalloc-rs, comparing it to the C `libmimalloc` reference and the system
# allocator via LD_PRELOAD. This is the allocator world's de-facto perf suite
# (cfrac, espresso, larson, mstress, rptest, xmalloc-test, …).
#
# It is a **local / pinned-machine tool** — NOT wired into CI (the workloads are
# heavy and shared CI runners are far too noisy for trustworthy allocator timing,
# the same reason perf is gated on the pinned machine via scripts/perf_compare.sh).
#
# What it does:
#   1. Builds the mimalloc-rs `override` cdylib (the LD_PRELOAD shared lib).
#   2. Clones + builds daanx/mimalloc-bench (its benchmark programs) if needed.
#   3. Runs a curated, stable-argument subset of those programs under three
#      allocators — system (no preload), C libmimalloc, mimalloc-rs — and prints
#      a per-program wall-time + peak-RSS table.
#
# Driving the built binaries directly under LD_PRELOAD (rather than registering a
# custom allocator inside mimalloc-bench's bench.sh) keeps this robust across
# mimalloc-bench versions and reuses exactly the override path preload-check.sh
# verifies.
#
# Env:
#   MIMALLOC_C_LIB     dir with libmimalloc.so for the C comparison (optional;
#                      the C column is skipped if unset).
#   MIMALLOC_BENCH_DIR checkout/build dir for mimalloc-bench (default /tmp/mimalloc-bench).
#   BENCH_THREADS      thread count for the MT benchmarks (default: nproc, capped 8).
#
# Prereqs for building mimalloc-bench: git, cmake, a C/C++ toolchain, and the
# usual autotools the suite needs. If its build fails, the cdylib is still built
# and the script prints how to finish manually.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH_DIR="${MIMALLOC_BENCH_DIR:-/tmp/mimalloc-bench}"
THREADS="${BENCH_THREADS:-$(( $(nproc 2>/dev/null || echo 4) ))}"
[[ "$THREADS" -gt 8 ]] && THREADS=8

# --- 1. Build the mimalloc-rs override cdylib (same recipe as preload-check) ---
PRELOAD_TARGET="$REPO_ROOT/target/preload"
echo ">> building mimalloc-rs override cdylib ..."
RUSTFLAGS="--cfg override_export" \
    cargo rustc --release --features override --crate-type cdylib \
    --target-dir "$PRELOAD_TARGET" || { echo "FAIL: cdylib build"; exit 1; }
RS_SO="$PRELOAD_TARGET/release/libmimalloc_rs.so"
[[ -f "$RS_SO" ]] || { echo "FAIL: cdylib not found at $RS_SO"; exit 1; }
echo "   rs:  $RS_SO"

C_SO=""
if [[ -n "${MIMALLOC_C_LIB:-}" && -f "${MIMALLOC_C_LIB}/libmimalloc.so" ]]; then
    C_SO="${MIMALLOC_C_LIB}/libmimalloc.so"
    echo "   c:   $C_SO"
else
    echo "   c:   (skipped — set MIMALLOC_C_LIB to a dir with libmimalloc.so to compare)"
fi

# --- 2. Clone + build mimalloc-bench -----------------------------------------
if [[ ! -d "$BENCH_DIR/.git" ]]; then
    echo ">> cloning daanx/mimalloc-bench into $BENCH_DIR ..."
    git clone --depth 1 https://github.com/daanx/mimalloc-bench.git "$BENCH_DIR" \
        || { echo "FAIL: clone mimalloc-bench"; exit 1; }
fi
BIN_DIR="$BENCH_DIR/out/bench"
if [[ ! -d "$BIN_DIR" ]]; then
    echo ">> building mimalloc-bench programs (build-bench-env.sh bench) ..."
    ( cd "$BENCH_DIR" && ./build-bench-env.sh bench ) || {
        echo "WARN: mimalloc-bench build failed (missing build deps?)."
        echo "      Install its prerequisites and re-run, or build manually:"
        echo "        (cd $BENCH_DIR && ./build-bench-env.sh bench)"
        echo "      The cdylib is built at: $RS_SO"
        exit 1
    }
fi

# --- 3. Run a curated, stable-arg subset under each allocator ----------------
# name | binary | args... (only programs that exist are run; the rest are skipped).
ESPRESSO_IN="$BENCH_DIR/bench/espresso/largest.espresso"
declare -a BENCHES=(
    "cfrac|cfrac|17545186520808147889450131403073"
    "espresso|espresso|$ESPRESSO_IN"
    "larson|larson|5 8 1000 5000 100 4141 $THREADS"
    "mstress|mstress|$THREADS 50 25"
    "rptest|rptest|$THREADS 0 1 2 500 1000 100 8 16000"
    "xmalloc-test|xmalloc-test|-w $THREADS -t 5 -s 64"
)

run_one() {  # $1=label $2=so("" for system) $3=bin $4=args
    local so="$2" bin="$3" args="$4"
    local pre=(); [[ -n "$so" ]] && pre=(env "LD_PRELOAD=$so")
    # /usr/bin/time -v gives wall ("Elapsed") + "Maximum resident set size" (KB).
    local t; t="$(/usr/bin/time -v "${pre[@]}" "$bin" $args 2>&1 >/dev/null)" || { printf "  %-8s ERR\n" "$1"; return; }
    local wall rss
    wall="$(awk -F': ' '/Elapsed \(wall/{print $2}' <<<"$t")"
    rss="$(awk -F': ' '/Maximum resident set size/{print $2}' <<<"$t")"
    printf "  %-8s wall=%-10s rssKB=%-9s\n" "$1" "${wall:-?}" "${rss:-?}"
}

echo
echo "=== mimalloc-bench (threads=$THREADS) — system | C libmimalloc | mimalloc-rs ==="
for spec in "${BENCHES[@]}"; do
    IFS='|' read -r name binname args <<<"$spec"
    bin="$BIN_DIR/$binname"
    if [[ ! -x "$bin" ]]; then
        echo "[$name] (binary not built — skipped)"
        continue
    fi
    echo "[$name]"
    run_one "system" ""      "$bin" "$args"
    [[ -n "$C_SO"  ]] && run_one "mi-c"  "$C_SO"  "$bin" "$args"
    run_one "mi-rs"  "$RS_SO" "$bin" "$args"
done
echo
echo "Done. (Heavy + noisy — interpret on a quiet, pinned machine; take the best of"
echo "several runs. For the full suite/options see $BENCH_DIR/bench.sh.)"
