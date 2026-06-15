#!/usr/bin/env bash
# Real load stress — strict gates, high volume. Not in gate.sh or CI.
# Each scenario runs in an isolated subprocess; 30s recovery between runs.
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=lib/ui.sh
source "$(dirname "$0")/lib/ui.sh"

# Allow ephemeral ports to clear after prior bench runs.
sleep 30

demiurge_banner "DEMIURGE · load stress" \
  "mode    strict · zero errors required" \
  "repo    $(_ui_git_ref)" \
  "host    $(_ui_host_tag)"

bold "stress scenarios (release, strict)"
set +e
cargo run --release -q --package xtask -- load-bench --stress
stress_rc=$?
set -e

bold "stress report"
cargo run --release -q --package xtask -- load-report --stress

if [ "$stress_rc" -ne 0 ]; then
  demiurge_fail "STRESS FAILED — see target/load-bench/stress.pseudo"
  exit "$stress_rc"
fi

demiurge_pass "STRESS PASSED — see target/load-bench/stress.pseudo"
