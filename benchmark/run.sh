#!/usr/bin/env bash
# Holistic allocator comparison, normalized as speedup over glibc.
#
# Two halves, each a clean *same-binary* (allocator-swap) comparison:
#   * mimalloc-rs vs glibc  — OUR Rust ports, built once as the static
#     #[global_allocator] and once `--features bench-system` (System/glibc).
#   * mimalloc-c  vs glibc  — the ORIGINAL mimalloc-bench binaries, run under
#     `LD_PRELOAD=<mimalloc.so>` vs no preload.
#
# Comparing the two cross-language is only meaningful via the **speedup over
# glibc** (the two harnesses are different programs of the same pattern). Do NOT
# link mimalloc's `static.c` into the C benches and call it "mimalloc-c": those
# benches call `malloc`/`new`, which static-link to *glibc* unless interposed —
# that mistake makes the C column secretly glibc.
#
#   MISO=/path/to/libmimalloc.so CBUILD=/path/to/mimalloc-bench/bench/build \
#     bash run.sh
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPS="${REPS:-5}"
PIN="${PIN:-taskset -c 2-9}"
T="${T:-8}"
MISO="${MISO:-/tmp/mimalloc-c/build/libmimalloc.so}"   # a CMake-Release mimalloc .so
CBUILD="${CBUILD:-/tmp/mimalloc-bench/bench/build}"     # built mimalloc-bench binaries
OUT=/tmp/mrsb; mkdir -p "$OUT"

med(){ printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'; }
wall(){ $PIN /usr/bin/time -f '%e' "$@" 2>&1 >/dev/null | grep -oE '^[0-9.]+$' | tail -1; }
wallp(){ $PIN env LD_PRELOAD="$MISO" /usr/bin/time -f '%e' "$@" 2>&1 >/dev/null | grep -oE '^[0-9.]+$' | tail -1; }
rate(){ local re="$1"; shift; $PIN "$@" 2>&1 | grep -oE "$re" | grep -oE '[0-9.]+' | tail -1; }
ratep(){ local re="$1"; shift; $PIN env LD_PRELOAD="$MISO" "$@" 2>&1 | grep -oE "$re" | grep -oE '[0-9.]+' | tail -1; }
medwall(){ local v=(); for _ in $(seq 1 $REPS); do v+=("$(wall "$@")"); done; med "${v[@]}"; }
medwallp(){ local v=(); for _ in $(seq 1 $REPS); do v+=("$(wallp "$@")"); done; med "${v[@]}"; }

echo ">> building OUR benches: rs (peak) + system ..."
( cd "$HERE" && cargo build --release -q && for b in malloc_large cache_thrash alloc_test xmalloc_test larson; do cp "target/release/$b" "$OUT/rs_$b"; done )
( cd "$HERE" && cargo build --release -q --features bench-system && for b in malloc_large cache_thrash alloc_test xmalloc_test larson; do cp "target/release/$b" "$OUT/sys_$b"; done )

HAVE_C=0; [ -r "$MISO" ] && [ -x "$CBUILD/xmalloc-test" ] && HAVE_C=1
[ $HAVE_C = 0 ] && echo ">> (mimalloc-c column skipped: set MISO + CBUILD to a mimalloc .so and a built mimalloc-bench)"

sp(){ awk -v a="$1" -v b="$2" 'BEGIN{ if(b+0==0){print "-"}else{printf "%.2fx", a/b} }'; }

printf "\n%-13s %-7s %22s %22s\n" "" "" "--- mimalloc-rs (ours) ---" "--- mimalloc-c (official) ---"
printf "%-13s %-7s %9s %9s %5s %9s %9s %5s\n" workload metric rs glibc x mi-c glibc x
printf -- "------------- ------- --------- --------- ----- --------- --------- -----\n"

# throughput (higher=better)  — rs uses our binaries; mi-c uses official xmalloc-test/larson
rx=$(rate 'FREE_PER_SEC [0-9]+' "$OUT/rs_xmalloc_test" 8 3 64); sx=$(rate 'FREE_PER_SEC [0-9]+' "$OUT/sys_xmalloc_test" 8 3 64)
if [ $HAVE_C = 1 ]; then cgx=$(awk -v v="$(rate 'free/sec: [0-9.]+' "$CBUILD/xmalloc-test" -w $T -t 3 -s 64)" 'BEGIN{print v*1e6}'); cmx=$(awk -v v="$(ratep 'free/sec: [0-9.]+' "$CBUILD/xmalloc-test" -w $T -t 3 -s 64)" 'BEGIN{print v*1e6}'); else cgx=0; cmx=0; fi
printf "%-13s %-7s %9.3g %9.3g %5s %9.3g %9.3g %5s\n" xmalloc-test "f/s↑" "$rx" "$sx" "$(sp $rx $sx)" "$cmx" "$cgx" "$(sp $cmx $cgx)"

rl=$(rate 'OPS_PER_SEC [0-9]+' "$OUT/rs_larson" 5 8 1000 5000 100 4141 $T); sl=$(rate 'OPS_PER_SEC [0-9]+' "$OUT/sys_larson" 5 8 1000 5000 100 4141 $T)
if [ $HAVE_C = 1 ]; then cgl=$(rate 'Throughput =[ ]*[0-9.]+' "$CBUILD/larson" 5 8 1000 5000 100 4141 $T); cml=$(ratep 'Throughput =[ ]*[0-9.]+' "$CBUILD/larson" 5 8 1000 5000 100 4141 $T); else cgl=0; cml=0; fi
printf "%-13s %-7s %9.3g %9.3g %5s %9.3g %9.3g %5s\n" larson "ops/s↑" "$rl" "$sl" "$(sp $rl $sl)" "$cml" "$cgl" "$(sp $cml $cgl)"

# time-based (lower=better) — speedup = glibc/alloc
row(){ local n="$1" rb="$2" sb="$3" cbin="$4"; shift 4
  local r=$(medwall "$OUT/$rb" "$@") s=$(medwall "$OUT/$sb" "$@")
  local cg="-" cm="-" cgx=0 cmx=0
  if [ $HAVE_C = 1 ] && [ -n "$cbin" ]; then cgx=$(medwall "$CBUILD/$cbin" "$@"); cmx=$(medwallp "$CBUILD/$cbin" "$@"); cg=$cgx; cm=$cmx; fi
  printf "%-13s %-7s %9s %9s %5s %9s %9s %5s\n" "$n" "sec↓" "$r" "$s" "$(sp $s $r)" "$cm" "$cg" "$(sp $cgx $cmx)"
}
row alloc-test  rs_alloc_test  sys_alloc_test  alloc-test   $T
row malloc-large rs_malloc_large sys_malloc_large malloc-large
row cache-thrash rs_cache_thrash sys_cache_thrash cache-thrash $T 1000 1 1000000 $T

echo
echo "Each allocator is compared to glibc on its OWN faithful harness (same-binary"
echo "swap). Cross-language: compare the 'x' (speedup over glibc) columns, not the"
echo "absolute rs-vs-mi-c numbers (different programs of the same pattern)."
