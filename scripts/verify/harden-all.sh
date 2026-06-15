#!/usr/bin/env bash
# Die-hard Tiers 1–4: unit/integration + harden load + aggregate report.
set -euo pipefail
# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"

SKIP_LOAD=0
SKIP_STRESS=1
for arg in "$@"; do
  case "$arg" in
    --skip-load) SKIP_LOAD=1 ;;
    --with-stress) SKIP_STRESS=0 ;;
    -h | --help)
      cat <<'EOF'
usage: ./scripts/verify/harden-all.sh [--skip-load] [--with-stress]

  default       tiers 1–3 + tier4 harden load + aggregate report
  --skip-load   tiers 1–3 + report only (load/stress json must exist on disk)
  --with-stress also run tier4-stress.sh (strict volume suite, ~5+ min)
EOF
      exit 0
      ;;
  esac
done

cd "$VERIFY_ROOT"
verify_mkdir

verify_banner "verify · die-hard (Tiers 1–4)" \
  "mode    harden-all" \
  "skip_load=$SKIP_LOAD · with_stress=$((1 - SKIP_STRESS))"

"${BASH_SOURCE%/*}/tier1-admit.sh"
"${BASH_SOURCE%/*}/tier2-faults.sh"
"${BASH_SOURCE%/*}/tier3-proptest.sh"

if [ "$SKIP_LOAD" -eq 0 ]; then
  "${BASH_SOURCE%/*}/tier4-load.sh"
fi

if [ "$SKIP_STRESS" -eq 0 ]; then
  "${BASH_SOURCE%/*}/tier4-stress.sh"
fi

# Load/stress already executed above (or pre-existing JSON on disk).
"${BASH_SOURCE%/*}/report.sh" --skip-load

demiurge_pass "HARDEN ALL PASSED — target/harden-verify/report.pseudo"
