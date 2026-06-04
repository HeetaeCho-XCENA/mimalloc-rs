#!/usr/bin/env bash
# 3-way benchmark: mimalloc-rs (peak, static #[global_allocator]) vs Rust's
# System (glibc) vs mimalloc-c (peak, the original C bench linked with
# src/static.c -O3 -flto). The rs-vs-system pair is the SAME Rust binary built
# two ways (perfectly fair). The C-peak column uses the original mimalloc-bench
# source for the benches whose pattern + metric compare cleanly (matched params
# or rate-based throughput); alloc-test is rs-vs-system only (the C original
# hardcodes a far larger iteration count).
#
#   MI_SRC=~/repos/mimalloc-v3 CBENCH=/tmp/mimalloc-bench/bench bash run.sh
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
MI_SRC="${MI_SRC:-$HOME/repos/mimalloc-v3}"
CBENCH="${CBENCH:-/tmp/mimalloc-bench/bench}"
REPS="${REPS:-5}"
PIN="${PIN:-taskset -c 2-9}"
T="${T:-8}"          # threads for the MT benches
OUT=/tmp/mrsb; mkdir -p "$OUT"

med(){ printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'; }
wall(){ $PIN /usr/bin/time -f '%e' "$@" 2>&1 >/dev/null | grep -oE '^[0-9.]+$' | tail -1; }
parse(){ $PIN "$@" 2>&1 | grep -oE "$RE" | grep -oE '[0-9.]+' | tail -1; }  # RE set by caller

echo ">> building rs (peak) + system variants ..."
( cd "$HERE" && cargo build --release -q && for b in malloc_large cache_thrash alloc_test xmalloc_test larson; do cp "target/release/$b" "$OUT/rs_$b"; done )
( cd "$HERE" && cargo build --release -q --features bench-system && for b in malloc_large cache_thrash alloc_test xmalloc_test larson; do cp "target/release/$b" "$OUT/sys_$b"; done )

echo ">> building mimalloc-c peak variants (static.c -O3 -flto) ..."
CI="-O3 -DNDEBUG -flto -I $MI_SRC/include"
g++ $CI "$CBENCH/malloc-large/malloc-large.cpp" "$MI_SRC/src/static.c" -lpthread -latomic -o "$OUT/c_malloc_large" 2>/dev/null || echo "  (malloc-large C build failed)"
g++ $CI "$CBENCH/cache-thrash/cache-thrash.cpp" "$MI_SRC/src/static.c" -lpthread -latomic -o "$OUT/c_cache_thrash" 2>/dev/null || echo "  (cache-thrash C build failed)"
gcc $CI "$CBENCH/xmalloc-test/xmalloc-test.c" "$MI_SRC/src/static.c" -lpthread -latomic -o "$OUT/c_xmalloc_test" 2>/dev/null || echo "  (xmalloc-test C build failed)"
g++ $CI "$CBENCH/larson/larson.cpp" "$MI_SRC/src/static.c" -lpthread -latomic -o "$OUT/c_larson" 2>/dev/null || echo "  (larson C build failed)"

run_wall(){ local bin="$1"; shift; local v=(); for _ in $(seq 1 $REPS); do v+=("$(wall "$bin" "$@")"); done; med "${v[@]}"; }
run_rate(){ local re="$1" bin="$2"; shift 2; RE="$re"; local v=(); for _ in $(seq 1 $REPS); do v+=("$(parse "$bin" "$@")"); done; med "${v[@]}"; }

printf "\n%-14s %-8s %12s %12s %12s   %s\n" workload metric mimalloc-rs system mimalloc-c note
printf -- "-------------- -------- ------------ ------------ ------------   ----\n"

# --- time-based (lower = better); matched params ---
# malloc-large: 2000 iters (same in C and rs)
r=$(run_wall "$OUT/rs_malloc_large"); s=$(run_wall "$OUT/sys_malloc_large"); c=$( [ -x "$OUT/c_malloc_large" ] && run_wall "$OUT/c_malloc_large" || echo "-")
printf "%-14s %-8s %12s %12s %12s   %s\n" malloc-large "sec↓" "$r" "$s" "$c" "20×5-25MiB"
# cache-thrash: matched args  nthreads iters objSize reps [concurrency]
CT="$T 1000 1 1000000"
r=$(run_wall "$OUT/rs_cache_thrash" $CT); s=$(run_wall "$OUT/sys_cache_thrash" $CT); c=$( [ -x "$OUT/c_cache_thrash" ] && run_wall "$OUT/c_cache_thrash" $CT $T || echo "-")
printf "%-14s %-8s %12s %12s %12s   %s\n" cache-thrash "sec↓" "$r" "$s" "$c" "${T}T false-share"
# alloc-test: rs vs system only (C hardcodes 100M iters)
r=$(run_wall "$OUT/rs_alloc_test" $T); s=$(run_wall "$OUT/sys_alloc_test" $T)
printf "%-14s %-8s %12s %12s %12s   %s\n" alloc-test "sec↓" "$r" "$s" "-" "${T}T (C iters differ)"

# --- throughput (higher = better); rate compares across PRNG diffs ---
# xmalloc-test:  rs/sys print FREE_PER_SEC (abs); C prints 'free/sec: N M' (millions)
RE='FREE_PER_SEC [0-9.]+'
r=$(run_rate "$RE" "$OUT/rs_xmalloc_test"); s=$(run_rate "$RE" "$OUT/sys_xmalloc_test")
RE='free/sec: [0-9.]+'
c=$( [ -x "$OUT/c_xmalloc_test" ] && awk -v v="$(run_rate "$RE" "$OUT/c_xmalloc_test" -w $T -t 3 -s 64)" 'BEGIN{printf "%.3g", v*1e6}' || echo "-")
printf "%-14s %-8s %12s %12s %12s   %s\n" xmalloc-test "f/s↑" "$(awk -v v=$r 'BEGIN{printf "%.3g",v}')" "$(awk -v v=$s 'BEGIN{printf "%.3g",v}')" "$c" "${T}w prod/cons"
# larson:  rs/sys print OPS_PER_SEC; C prints 'Throughput = N operations'
RE='OPS_PER_SEC [0-9.]+'
LA="3 1000 5000 100 4141 1 $T"   # secs min max chunks rounds seed threads (7 args)
r=$(run_rate "$RE" "$OUT/rs_larson" $LA); s=$(run_rate "$RE" "$OUT/sys_larson" $LA)
RE='Throughput =[ ]*[0-9.]+'
c=$( [ -x "$OUT/c_larson" ] && run_rate "$RE" "$OUT/c_larson" $LA || echo "-")
printf "%-14s %-8s %12s %12s %12s   %s\n" larson "ops/s↑" "$(awk -v v=$r 'BEGIN{printf "%.3g",v}')" "$(awk -v v=$s 'BEGIN{printf "%.3g",v}')" "$(awk -v v=$c 'BEGIN{printf "%.3g",v}')" "${T}T server"

echo
echo "rs-vs-system: identical Rust binary (fair). mimalloc-c: original bench + static.c -O3 -flto (peak)."
