#!/usr/bin/env bash
# Aggregate Tier 1–4 results into target/harden-verify/report.pseudo (+ report.md).
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"
cd "$VERIFY_ROOT"
verify_mkdir

SKIP_LOAD=0
for arg in "$@"; do
  case "$arg" in
    --skip-load) SKIP_LOAD=1 ;;
    -h | --help)
      echo "usage: $0 [--skip-load]"
      echo "  --skip-load  aggregate only (use after verify load/stress already ran)"
      exit 0
      ;;
  esac
done

verify_banner "verify · aggregate report" "output  target/harden-verify/report.pseudo"

set +e
cargo run --release -q --package xtask -- harden-verify --skip-tests --skip-load \
  2>&1 | tee "$VERIFY_OUT/reports/harden-verify.log"
rc=$?
set -e

if [ -f target/harden-verify/report.pseudo ]; then
  cp -f target/harden-verify/report.pseudo "$VERIFY_OUT/reports/report.pseudo"
  cp -f target/harden-verify/report.md "$VERIFY_OUT/reports/report.md" 2>/dev/null || true
  bold "pseudo report"
  cat target/harden-verify/report.pseudo
fi

if [ "$rc" -ne 0 ]; then
  demiurge_fail "REPORT FAILED — see target/harden-verify/report.pseudo"
  exit "$rc"
fi

demiurge_pass "REPORT READY — target/harden-verify/report.pseudo"
