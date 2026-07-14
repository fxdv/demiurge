#!/usr/bin/env bash
# Demiurge verification suite — single entry for all rerunnable checks.
#
#   ./scripts/verify.sh list                 show suites + artifact paths
#   ./scripts/verify.sh gate [--quick]       CI mirror (default pre-push hook)
#   ./scripts/verify.sh harden [--skip-load] [--with-stress]   Tiers 1–4 die-hard
#   ./scripts/verify.sh tier{N}              run one harden tier (1–4)
#   ./scripts/verify.sh load                 full load bench + pseudo report
#   ./scripts/verify.sh stress               strict stress suite + pseudo report
#   ./scripts/verify.sh sim                  'sim fleet simulation spinoff
#   ./scripts/verify.sh track-a              Track A metrics + load + stress
#   ./scripts/verify.sh track-b [--quick]    Track B (Linux only)
#   ./scripts/verify.sh track-c [--quick]  Track C P/D proof (Linux + GPU fleet)
#   ./scripts/verify.sh pre-release          nightly / linux-nightly validation
#   ./scripts/verify.sh full                 gate + load + stress + harden + 'sim
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=lib/ui.sh
source "$(dirname "$0")/lib/ui.sh"

VERIFY_DIR="$(dirname "$0")/verify"

usage() {
  sed -n '2,12p' "$0"
  echo ""
  echo "Artifacts:"
  echo "  target/verify/tier{1,2,3,4}/     per-tier logs"
  echo "  target/harden-verify/            die-hard pseudo + markdown report"
  echo "  target/track-a-verify/           Track A report.md"
  echo "  target/track-b-verify/           Track B report.md (Linux)"
  echo "  target/track-c-verify/           Track C report.md (Linux + GPU fleet)"
  echo "  target/load-bench/               load + stress + harden + sim JSON/pseudo"
  echo "  target/reports/                  sim disclosure archive (sim-latest.*)"
  echo ""
  echo "Layout: DEMIURGE_UI_WIDTH / DEMIURGE_PSEUDO_WIDTH (default 120, max 200)"
}

cmd=${1:-list}
shift || true

case "$cmd" in
  list | help | -h | --help)
    usage
    ;;
  gate)
    exec ./scripts/gate.sh "$@"
    ;;
  harden)
    exec "$VERIFY_DIR/harden-all.sh" "$@"
    ;;
  tier1 | tier1-admit)
    exec "$VERIFY_DIR/tier1-admit.sh"
    ;;
  tier2 | tier2-faults)
    exec "$VERIFY_DIR/tier2-faults.sh"
    ;;
  tier3 | tier3-proptest)
    exec "$VERIFY_DIR/tier3-proptest.sh"
    ;;
  tier4 | tier4-load)
    exec "$VERIFY_DIR/tier4-load.sh"
    ;;
  tier4-stress | stress-tier4)
    exec "$VERIFY_DIR/tier4-stress.sh"
    ;;
  report | harden-report)
    exec "$VERIFY_DIR/report.sh" "$@"
    ;;
  load)
    exec ./scripts/load-bench.sh
    ;;
  stress)
    exec ./scripts/load-stress.sh
    ;;
  sim | apostrophe-sim | "'sim")
    exec ./scripts/apostrophe-sim.sh
    ;;
  track-a)
    exec ./scripts/track-a-verify.sh "$@"
    ;;
  track-b)
    exec ./scripts/track-b-verify.sh "$@"
    ;;
  track-c)
    exec ./scripts/track-c-verify.sh "$@"
    ;;
  pre-release)
    exec ./scripts/pre-release.sh
    ;;
  full)
    demiurge_banner "DEMIURGE · full verification" \
      "repo    $(_ui_git_ref)" \
      "host    $(_ui_host_tag)"
    ./scripts/gate.sh
    ./scripts/load-bench.sh
    ./scripts/load-stress.sh
    "$VERIFY_DIR/tier4-load.sh"
    "$VERIFY_DIR/harden-all.sh" --skip-load --with-stress
    ./scripts/apostrophe-sim.sh
    ./scripts/generate-sim-report.sh --skip-run
    if [[ "$(uname -s)" == "Linux" ]]; then
      echo ""
      echo "Track B extras (Linux): ./scripts/verify.sh track-b --quick"
    else
      echo ""
      echo "Track A extras (macOS): ./scripts/verify.sh track-a"
    fi
    demiurge_pass "FULL VERIFY PASSED (gate + load + stress + harden + 'sim)"
    ;;
  *)
    echo "unknown verify command: $cmd (try: ./scripts/verify.sh list)" >&2
    exit 2
    ;;
esac
