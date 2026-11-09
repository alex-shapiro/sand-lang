#!/usr/bin/env bash
# A/B/C overhead of the prototypes on code that does not use callbacks:
# ROUNDS (default 3) interleaved rounds of bench/run.py's programs suite per binary.
# Usage: overhead.sh BASE_SAND P2_SAND P1_SAND
set -eu
cd "$(dirname "$0")/../../.."
PY=$(uv python find 3.14)
out=research/native-callbacks/lab/results/overhead
mkdir -p "$out"
for round in $(seq 1 "${ROUNDS:-3}"); do
  for pair in "base:$1" "p2:$2" "p1:$3"; do
    name=${pair%%:*}; bin=${pair#*:}
    $PY bench/run.py --suite programs --impls sand --sand "$bin" --runs 5 > "$out/$name-round$round.txt" 2>/dev/null
  done
done
