#!/usr/bin/env bash
# Pre-release / nightly validation — strict gates + full load + stress + actuation.
# Not in CI: LOAD-STEP-ACTUATE and stress scenarios use 30s port-recovery pauses.
set -euo pipefail
cd "$(dirname "$0")/.."

mkdir -p target/release-artifacts

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

bold "gate (CI mirror + required Track B on Linux)"
./scripts/gate.sh 2>&1 | tee target/release-artifacts/gate.log

bold "full load bench (includes LOAD-STEP-ACTUATE + isolate_recovery scenarios)"
./scripts/load-bench.sh
cp target/load-bench/latest.json target/load-bench/load-full.json

bold "stress suite (strict zero-error)"
./scripts/load-stress.sh

bold "die-hard verify (Tiers 1–4 observable report)"
./scripts/verify/harden-all.sh --skip-load --with-stress

printf '\n\033[1;32mPRE-RELEASE PASSED\033[0m\n'
