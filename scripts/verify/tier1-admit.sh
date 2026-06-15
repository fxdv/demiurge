#!/usr/bin/env bash
# Tier 1 — admit/shed: token lifecycle + wire-level 503.
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"
cd "$VERIFY_ROOT"
verify_mkdir

verify_banner "verify · Tier 1 admit/shed" "scope   DEMI-XDP-SHED · wire 503"

bold "harden admit/shed integration tests"
verify_cargo_tests "$VERIFY_OUT/tier1/harden.log" demiurge-router harden \
  harden_tcp_503_on_admit_exhaust \
  harden_admit_conn_single_guard_path \
  harden_kv_admit_rejected_returns_503

bold "p5 dataplane admit tests"
verify_cargo_tests "$VERIFY_OUT/tier1/p5-admit.log" demiurge-router p5_dataplane \
  userspace_admit_limits_concurrent_connections \
  admit_bucket_sheds_on_live_router

bold "admit bucket unit test"
verify_cargo_tests "$VERIFY_OUT/tier1/admission.log" demiurge-dataplane admission \
  admit_bucket_sheds_when_exhausted

demiurge_pass "TIER 1 PASSED — logs in target/verify/tier1/"
