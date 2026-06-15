#!/usr/bin/env bash
# Local load bench + pseudo-graphical report (post-step).
# Not part of gate.sh — spins up TCP servers and takes a few seconds.
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=lib/ui.sh
source "$(dirname "$0")/lib/ui.sh"

demiurge_banner "DEMIURGE · load bench" \
  "mode    local scenarios · release" \
  "repo    $(_ui_git_ref)" \
  "host    $(_ui_host_tag)" \
  "note    proof ≠ production · mock TCP backends"

bold "load scenarios (release)"
set +e
cargo run --release -q --package xtask -- load-bench
bench_rc=$?
set -e

bold "pseudo report (post-step)"
cargo run --release -q --package xtask -- load-report

if [ "$bench_rc" -ne 0 ]; then
  printf '\n\033[1;31mLOAD BENCH: FAILED\033[0m (strict gate or isolated scenario — see report)\033[0m\n' >&2
  exit "$bench_rc"
fi

demiurge_pass "LOAD BENCH DONE — see target/load-bench/latest.pseudo"
