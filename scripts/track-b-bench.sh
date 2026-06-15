#!/usr/bin/env bash
# Track B CPU + runtime micro-benches (Linux). Faster than full track-b-verify.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "track-b-bench: Linux only" >&2
  exit 1
fi

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

bash ./scripts/ensure-rust-toolchain.sh

bold "CPU bench-probe (headroom)"
cargo run --release -q --package xtask -- bench-probe

bold "CPU bench-gate (release hot paths incl. BENCH-IOURING-FWD)"
cargo run --release -q --package xtask -- bench-gate

bold "Track B runtime gate (BPF + XDP veth smoke)"
./scripts/track-b-gate.sh

printf '\n\033[1;32mTRACK B BENCH: PASSED\033[0m\n'
