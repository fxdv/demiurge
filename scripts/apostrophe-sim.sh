#!/usr/bin/env bash
# 'sim — Demiurge fleet simulation spinoff (trace replay + mock fleet).
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=lib/ui.sh
source "$(dirname "$0")/lib/ui.sh"

demiurge_banner "DEMIURGE · 'sim" \
  "mode    trace-driven fleet replay · release" \
  "repo    $(_ui_git_ref)" \
  "host    $(_ui_host_tag)" \
  "note    spinoff · mock TCP · proof ≠ GPU production"

bold "fleet simulation (release)"
set +e
cargo run --release -q --package xtask -- apostrophe-sim
sim_rc=$?
set -e

if [ "$sim_rc" -ne 0 ]; then
  printf '\n\033[1;31m\x27sim: FAILED\033[0m — see target/load-bench/sim.pseudo\033[0m\n' >&2
  exit "$sim_rc"
fi

demiurge_pass "'sim PASSED — see target/load-bench/sim.pseudo"
