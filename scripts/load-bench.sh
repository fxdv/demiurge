#!/usr/bin/env bash
# Local load bench + pseudo-graphical report (post-step).
# Not part of gate.sh — spins up TCP servers and takes a few seconds.
set -euo pipefail
cd "$(dirname "$0")/.."

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

bold "load scenarios (release)"
set +e
cargo run --release -q --package xtask -- load-bench
bench_rc=$?
set -e

bold "pseudo report (post-step)"
cargo run --release -q --package xtask -- load-report

if [ "$bench_rc" -ne 0 ]; then
  printf '\n\033[1;33mLOAD BENCH: soft gate failure (see report)\033[0m\n' >&2
  exit "$bench_rc"
fi

printf '\n\033[1;32mLOAD BENCH DONE\033[0m — see target/load-bench/latest.pseudo\n'
