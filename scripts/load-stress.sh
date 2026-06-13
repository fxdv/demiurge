#!/usr/bin/env bash
# Real load stress — strict gates, high volume. Not in gate.sh or CI.
# Each scenario runs in an isolated subprocess; 30s recovery between runs.
set -euo pipefail
cd "$(dirname "$0")/.."

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

# Allow ephemeral ports to clear after prior bench runs.
sleep 30

bold "stress scenarios (release, strict)"
set +e
cargo run --release -q --package xtask -- load-bench --stress
stress_rc=$?
set -e

bold "stress report"
cargo run --release -q --package xtask -- load-report --stress

if [ "$stress_rc" -ne 0 ]; then
  printf '\n\033[1;31mSTRESS FAILED\033[0m — see target/load-bench/stress.pseudo\n' >&2
  exit "$stress_rc"
fi

printf '\n\033[1;32mSTRESS PASSED\033[0m — see target/load-bench/stress.pseudo\n'
