#!/usr/bin/env bash
# Pre-release / nightly validation — strict gates + full load + stress + actuation.
# Not in CI: LOAD-STEP-ACTUATE and stress scenarios use 30s port-recovery pauses.
set -euo pipefail
cd "$(dirname "$0")/.."

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

bold "gate (CI mirror)"
./scripts/gate.sh

bold "full load bench (includes LOAD-STEP-ACTUATE + isolate_recovery scenarios)"
./scripts/load-bench.sh

bold "stress suite (strict zero-error)"
./scripts/load-stress.sh

printf '\n\033[1;32mPRE-RELEASE PASSED\033[0m\n'
