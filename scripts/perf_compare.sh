#!/usr/bin/env bash
# Authoritative no-regression comparison for the V-round (V1 lock-free abandoned
# stack + V2 lazy full-page eviction) vs the baseline `main`.
#
# Builds the release `bench_suite` for BOTH the current branch and `main`
# (in a throwaway git worktree), runs them interleaved on a pinned core for
# `REPS` repetitions, and prints a per-phase min/median table with the percent
# delta (branch - main). A phase regresses only if BOTH the min and the median
# rise beyond the noise threshold across enough reps — the single-thread and
# cross-thread phases are inherently noisy, so prefer the min and run REPS>=9.
#
# Usage:
#   MIMALLOC_C_LIB=/path/to/mimalloc-v3/build-diff scripts/perf_compare.sh [REPS] [CORE]
#
# Env:
#   MIMALLOC_C_LIB  dir containing libmimalloc.so (the C v3 reference); required
#                   by bench_suite to load the C target. The rs-vs-rs comparison
#                   here only reads the `mimalloc-rs` lines, but the harness
#                   still loads the C lib at startup.
#   REPS  (arg 1)   repetitions per binary (default 9)
#   CORE  (arg 2)   CPU core to pin to (default 2)
set -euo pipefail

REPS="${1:-9}"
CORE="${2:-2}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${MIMALLOC_C_LIB:?set MIMALLOC_C_LIB to the C v3 build dir (contains libmimalloc.so)}"

cd "$ROOT"
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
echo "branch = $BRANCH   baseline = main   reps = $REPS   core = $CORE"

# Build branch bench_suite (first build is a full release compile — minutes —
# so show progress rather than appear hung).
echo ">> building branch ($BRANCH) bench_suite ... (first run compiles the whole crate)"
cargo build --release --example bench_suite
# rm before cp: if a previous run left a still-running binary, unlinking it
# gives a fresh inode (cp into a busy executable fails with ETXTBSY).
rm -f /tmp/bs_branch
cp target/release/examples/bench_suite /tmp/bs_branch

# Build main bench_suite in a throwaway worktree (separate target dir → another
# full compile the first time). Use --detach so we check out main's *commit*
# without "using" the main branch — this never conflicts with the main checkout
# or a stale worktree. Prune dead entries from any earlier interrupted run first.
git worktree prune
WT="$(mktemp -u /tmp/mrs-main.XXXX)"   # -u: a NAME only; git worktree add creates it
echo ">> adding detached worktree for main at $WT and building its bench_suite ..."
git worktree add -q --detach "$WT" main
( cd "$WT" && cargo build --release --example bench_suite )
rm -f /tmp/bs_main
cp "$WT/target/release/examples/bench_suite" /tmp/bs_main
echo ">> both binaries built; running $REPS interleaved reps on core $CORE ..."

run() { taskset -c "$CORE" "$1" 2>&1 | grep -E '^(== phase|mimalloc-rs)'; }

: > /tmp/cmp_main.txt
: > /tmp/cmp_branch.txt
for _ in $(seq 1 "$REPS"); do
  run /tmp/bs_main   >> /tmp/cmp_main.txt
  run /tmp/bs_branch >> /tmp/cmp_branch.txt
done

python3 - <<'PY'
import re, statistics
def parse(fn):
    res, cur = {}, None
    for line in open(fn):
        m = re.search(r'== (phase \w+)', line)
        if m: cur = m.group(1); continue
        if 'mimalloc-rs' not in line: continue
        if 'threads' in line or 'pairs' in line:
            mm = re.search(r'(\d+) (threads|pairs).*?min\s+([0-9.]+)ms', line)
            if mm: res.setdefault(f"{cur} {mm.group(1)}{mm.group(2)[0]}", []).append(float(mm.group(3)))
        else:
            mm = re.search(r'min\s+([0-9.]+)ms', line)
            if mm: res.setdefault(cur, []).append(float(mm.group(1)))
    return res
m, b = parse('/tmp/cmp_main.txt'), parse('/tmp/cmp_branch.txt')
print(f"{'phase':<16}{'main min/med':>16}{'branch min/med':>18}{'Δmin%':>8}{'Δmed%':>8}  verdict")
worst = 0.0
for k in m:
    if k not in b: continue
    mn, md = min(m[k]), statistics.median(m[k])
    bn, bd = min(b[k]), statistics.median(b[k])
    dmin, dmed = (bn-mn)/mn*100, (bd-md)/md*100
    v = 'REGRESS' if (dmin > 3 and dmed > 3) else ('faster' if dmed < -3 else 'ok')
    worst = max(worst, min(dmin, dmed))
    print(f"{k:<16}{mn:>7.2f}/{md:>6.2f}{bn:>9.2f}/{bd:>6.2f}{dmin:>+7.1f}{dmed:>+7.1f}  {v}")
print(f"\nworst conservative delta (min of Δmin,Δmed per phase, max over phases): {worst:+.1f}%")
print("MERGE only if no phase shows REGRESS (both Δmin and Δmed > +3%).")
PY

git worktree remove --force "$WT" 2>/dev/null || true
rm -rf "$WT"
