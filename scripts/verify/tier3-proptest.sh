#!/usr/bin/env bash
# Tier 3 — property tests (admit bucket + reservation ledger).
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"
cd "$VERIFY_ROOT"
verify_mkdir

verify_banner "verify · Tier 3 proptest" "scope   AdmitBucket · ReservationLedger"

verify_run "$VERIFY_OUT/tier3/admit-proptest.log" \
  cargo test -p demiurge-dataplane admit_bucket_invariants -- --nocapture

verify_run "$VERIFY_OUT/tier3/ledger-proptest.log" \
  cargo test -p demiurge-control reservation_ledger_invariants -- --nocapture

verify_run "$VERIFY_OUT/tier3/bpf-admit-model.log" \
  cargo test -p demiurge-dataplane bpf_admit_model -- --nocapture

demiurge_pass "TIER 3 PASSED — logs in target/verify/tier3/"
