#!/usr/bin/env bash
# Pre-release / linux-nightly validation — full verify (incl. 'sim).
set -euo pipefail
cd "$(dirname "$0")/.."

mkdir -p target/release-artifacts

./scripts/verify.sh full

printf '\n\033[1;32mPRE-RELEASE PASSED\033[0m\n'
