#!/usr/bin/env bash
# Tier 2 — fault paths, hybrid admit, actuation, io_uring edges (Linux).
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"
cd "$VERIFY_ROOT"
verify_mkdir

verify_banner "verify · Tier 2 fault paths" "scope   KV/RST/hybrid/actuation/io_uring"

bold "harden fault-path integration tests"
verify_cargo_tests "$VERIFY_OUT/tier2/harden.log" demiurge-router harden \
  harden_handoff_duplicate_rejects_and_releases \
  harden_hybrid_admit_mode_matrix \
  harden_step_actuation_raises_dataplane_pi \
  harden_backend_rst_releases_admit_token \
  harden_kv_over_capacity_rejected

if verify_is_linux; then
  bold "io_uring harden tests (Linux)"
  verify_run "$VERIFY_OUT/tier2/io_uring.log" \
    cargo test -p demiurge-router --test forward_io_uring harden_ -- --nocapture
else
  bold "skip io_uring harden tests — not Linux"
  echo "SKIP io_uring (Linux only)" >"$VERIFY_OUT/tier2/io_uring.log"
fi

demiurge_pass "TIER 2 PASSED — logs in target/verify/tier2/"
