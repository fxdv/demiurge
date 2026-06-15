#!/usr/bin/env bash
# Tier 4 stress — strict volume suite (LOAD-STRESS-* + admit flood).
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"
cd "$VERIFY_ROOT"
verify_mkdir

verify_banner "verify · Tier 4 stress" \
  "mode    strict · zero hard errors" \
  "note    30s recovery between isolated scenarios"

bold "port recovery pause (30s)"
sleep 30

bold "stress scenarios (release, strict)"
set +e
cargo run --release -q --package xtask -- load-bench --stress \
  2>&1 | tee "$VERIFY_OUT/tier4/load-stress.log"
rc=$?
set -e

bold "stress pseudo report"
cargo run --release -q --package xtask -- load-report --stress \
  2>&1 | tee "$VERIFY_OUT/tier4/stress-report.log"
cp -f target/load-bench/stress.json "$VERIFY_OUT/tier4/stress.json" 2>/dev/null || true
cp -f target/load-bench/stress.pseudo "$VERIFY_OUT/tier4/stress.pseudo" 2>/dev/null || true

if [ "$rc" -ne 0 ]; then
  demiurge_fail "TIER 4 STRESS FAILED — see target/verify/tier4/load-stress.log"
  exit "$rc"
fi

demiurge_pass "TIER 4 STRESS PASSED — target/verify/tier4/stress.pseudo"
