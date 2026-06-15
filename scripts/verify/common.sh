#!/usr/bin/env bash
# Shared helpers for scripts/verify/* (source only — do not execute directly).
set -euo pipefail

VERIFY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERIFY_OUT="${VERIFY_ROOT}/target/verify"

# shellcheck source=../lib/ui.sh
source "${VERIFY_ROOT}/scripts/lib/ui.sh"

verify_mkdir() {
  mkdir -p "$VERIFY_OUT"/{tier1,tier2,tier3,tier4,reports}
  mkdir -p "${VERIFY_ROOT}/target/harden-verify"
}

verify_banner() {
  local title=$1
  shift
  demiurge_banner "DEMIURGE · ${title}" \
    "repo    $(_ui_git_ref)" \
    "host    $(_ui_host_tag)" \
    "$@"
}

verify_run() {
  local log=$1
  shift
  bold "$*"
  "$@" 2>&1 | tee "$log"
}

# Run several #[test] names (cargo accepts one filter per invocation).
verify_cargo_tests() {
  local log=$1 pkg=$2 test_target=$3
  shift 3
  local -a tests=("$@")
  : >"$log"
  local t
  for t in "${tests[@]}"; do
    echo "=== ${pkg} ${test_target:+--test $test_target }${t} ===" | tee -a "$log"
    if [[ -n "$test_target" ]]; then
      cargo test -p "$pkg" --test "$test_target" "$t" -- --nocapture 2>&1 | tee -a "$log"
    else
      cargo test -p "$pkg" "$t" -- --nocapture 2>&1 | tee -a "$log"
    fi
  done
}

verify_is_linux() {
  [[ "$(uname -s)" == "Linux" ]]
}
