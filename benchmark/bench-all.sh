#!/usr/bin/env bash
# Portable runner for the whole mimalloc-rs benchmark suite.
#
# Runs every workload twice from the SAME source — once with mimalloc-rs as the
# static #[global_allocator] (its peak) and once with Rust's System allocator
# (glibc) — so the rs-vs-glibc comparison is a clean same-binary swap. If a
# mimalloc shared library is available it adds a mimalloc-c column by
# LD_PRELOAD-ing it over the system build (works for every workload, since they
# are all our own binaries).
#
# Only needs `cargo` and this repo. The mimalloc-c column is optional.
#
# Usage:
#   bash benchmark/bench-all.sh
#
# Env knobs (all optional):
#   T=8            threads for the multi-threaded workloads
#   REPS=5         repetitions per workload (median is reported)
#   SCALE=1        multiplier on iteration counts (raise on a fast box, lower to
#                  finish quicker; e.g. SCALE=0.25 or SCALE=4)
#   PIN=""         CPU-pin prefix for steadier numbers, e.g. PIN="taskset -c 2-9"
#   CT_OBJ=16      cache-thrash object size in bytes (1 = the noisy tiny case)
#   MI_SO=/path/libmimalloc.so   add a mimalloc-c column from this prebuilt .so
#   MI_SRC=/path/to/mimalloc     ...or build that column from a checkout (cmake)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
T="${T:-8}"; REPS="${REPS:-5}"; SCALE="${SCALE:-1}"; PIN="${PIN:-}"; CT_OBJ="${CT_OBJ:-16}"
BINS="alloc_test xmalloc_test larson cache_thrash malloc_large calloc_test realloc_test"
OUT="$(mktemp -d)"; trap 'rm -rf "$OUT"' EXIT

command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found — install Rust (https://rustup.rs)"; exit 1; }

# integer-scale an iteration count by SCALE (floor, min 1)
sc(){ awk -v v="$1" -v s="$SCALE" 'BEGIN{r=v*s; printf "%d", (r<1)?1:r}'; }

echo ">> building benches: mimalloc-rs (peak) + system(glibc) ..."
( cd "$HERE" && cargo build --release -q )
for b in $BINS; do cp "$HERE/target/release/$b" "$OUT/rs_$b"; done
( cd "$HERE" && cargo build --release -q --features bench-system )
for b in $BINS; do cp "$HERE/target/release/$b" "$OUT/sys_$b"; done

# Optional mimalloc-c shared library.
SO=""
if [ -n "${MI_SO:-}" ] && [ -r "${MI_SO:-}" ]; then
  SO="$MI_SO"
elif [ -n "${MI_SRC:-}" ]; then
  if command -v cmake >/dev/null 2>&1; then
    echo ">> building mimalloc-c from $MI_SRC (cmake Release) ..."
    bd="$OUT/mic-build"; mkdir -p "$bd"
    ( cd "$bd" && cmake -DCMAKE_BUILD_TYPE=Release "$MI_SRC" >/dev/null && cmake --build . -j"$(nproc)" >/dev/null )
    SO="$(ls "$bd"/libmimalloc.so* 2>/dev/null | head -1 || true)"
  else
    echo ">> cmake not found — skipping the mimalloc-c column"
  fi
fi
if [ -n "$SO" ]; then echo ">> mimalloc-c column via LD_PRELOAD=$SO"
else echo ">> rs-vs-glibc only (set MI_SO=<libmimalloc.so> or MI_SRC=<checkout> to add mimalloc-c)"; fi

med(){ printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR? a[int((NR+1)/2)] : 0)}'; }
# Extract the workload's reported metric (SECONDS / FREE_PER_SEC / OPS_PER_SEC).
val(){ local pre="$1" bin="$2"; shift 2
  $PIN env ${pre:+LD_PRELOAD="$pre"} "$bin" "$@" 2>&1 \
    | grep -oE '(SECONDS|FREE_PER_SEC|OPS_PER_SEC) [0-9.]+' | grep -oE '[0-9.]+$' | tail -1; }
medval(){ local pre="$1" bin="$2"; shift 2; local v=(); for _ in $(seq 1 "$REPS"); do v+=("$(val "$pre" "$bin" "$@")"); done; med "${v[@]}"; }
ratio(){ awk -v a="$1" -v b="$2" 'BEGIN{if(b+0==0){print "-"}else{printf "%.2fx", a/b}}'; }

# $1 name  $2 bin  $3 dir(lo|hi)  $4.. args
row(){ local name="$1" bin="$2" dir="$3"; shift 3
  local r s m="-" mx=0
  r=$(medval "" "$OUT/rs_$bin" "$@"); s=$(medval "" "$OUT/sys_$bin" "$@")
  [ -n "$SO" ] && { mx=$(medval "$SO" "$OUT/sys_$bin" "$@"); m=$mx; }
  local unit rg mg="-"
  if [ "$dir" = lo ]; then unit="sec↓"; rg=$(ratio "$s" "$r"); [ -n "$SO" ] && mg=$(ratio "$s" "$mx")
  else unit="ops/s↑"; rg=$(ratio "$r" "$s"); [ -n "$SO" ] && mg=$(ratio "$mx" "$s"); fi
  printf "%-13s %-7s %11s %11s %6s %11s %6s\n" "$name" "$unit" "$r" "$s" "$rg" "$m" "$mg"
}

echo
echo "host: $(uname -sm), $(nproc) CPUs   |   T=$T REPS=$REPS SCALE=$SCALE CT_OBJ=$CT_OBJ ${PIN:+PIN=\"$PIN\"}"
printf "%-13s %-7s %11s %11s %6s %11s %6s\n" workload metric rs glibc rs/g "mi-c" mic/g
printf -- '------------- ------- ----------- ----------- ------ ----------- ------\n'
row alloc-test   alloc_test   lo "$T" "$(sc 4000000)"
row xmalloc-test xmalloc_test hi "$T" 3 64
row larson       larson       hi 5 8 1000 5000 100 4141 "$T"
row cache-thrash cache_thrash lo "$T" "$(sc 1000)" "$CT_OBJ" "$(sc 1000000)"
row malloc-large malloc_large lo "$(sc 2000)"
row calloc-test  calloc_test  lo "$T" "$(sc 100000)" 64
row realloc-test realloc_test lo "$T" "$(sc 3000000)"
echo
echo "rs/g and mic/g are speedups over glibc (>1 = faster than glibc). rs and the"
echo "mi-c column are the same binary swapped/preloaded, so they compare directly."
echo "calloc-test touches every page (real first-touch cost is included)."
