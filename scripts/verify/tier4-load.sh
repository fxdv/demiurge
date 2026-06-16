#!/usr/bin/env bash
# Tier 4 — harden load scenarios (LOAD-KV-EXHAUST, LOAD-RDMA-TOPO, LOAD-IOURING-LARGE-BODY on Linux).
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"
cd "$VERIFY_ROOT"
verify_mkdir

verify_banner "verify · Tier 4 harden load" \
  "scope   load-bench --harden" \
  "note    proof ≠ production · mock TCP backends"

bold "harden load scenarios (release, strict)"
set +e
cargo run --release -q --package xtask -- load-bench --harden \
  2>&1 | tee "$VERIFY_OUT/tier4/load-harden.log"
rc=$?
set -e

bold "harden load pseudo report"
cargo run --release -q --package xtask -- load-report --harden \
  2>&1 | tee "$VERIFY_OUT/tier4/load-harden-report.log"
cp -f target/load-bench/harden.json "$VERIFY_OUT/tier4/harden.json" 2>/dev/null || true
cp -f target/load-bench/harden.pseudo "$VERIFY_OUT/tier4/harden.pseudo" 2>/dev/null || true

if [ "$rc" -ne 0 ]; then
  demiurge_fail "TIER 4 FAILED — see target/verify/tier4/load-harden.log"
  exit "$rc"
fi

demiurge_pass "TIER 4 PASSED — target/verify/tier4/harden.pseudo"
