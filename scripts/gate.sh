#!/usr/bin/env bash
# Local gate — mirrors CI design-conformance + quality checks.
#
#   ./scripts/gate.sh          # full CI mirror (default; pre-push hook)
#   ./scripts/gate.sh --quick  # inner loop: gen, drift, lint, fmt, clippy, test
set -euo pipefail
cd "$(dirname "$0")/.."

QUICK=0
for arg in "$@"; do
  case "$arg" in
    --quick) QUICK=1 ;;
    -h | --help)
      sed -n '1,8p' "$0"
      echo ""
      echo "  --quick   skip release build, bench gates, load smoke, fleet-pilot, Track B, spec PDF"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg (try --quick)" >&2
      exit 2
      ;;
  esac
done

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

bold "regenerate artifacts from canonical inputs"
cargo xtask gen

bold "drift check (generated files must match canonical inputs)"
if [[ -d .git ]]; then
  if ! git diff --quiet -- spec/generated crates/demiurge-cost/src/generated_params.rs; then
    echo "ERROR: generated artifacts are stale — run 'cargo xtask gen' and commit:" >&2
    git --no-pager diff --stat -- spec/generated crates/demiurge-cost/src/generated_params.rs >&2
    exit 1
  fi
else
  echo "skip — no .git (e.g. Vagrant rsync); run gate on host before push"
fi

bold "traceability lint (spec <-> code <-> test)"
cargo xtask lint

bold "format check"
cargo fmt --all -- --check

bold "clippy (warnings are errors)"
cargo clippy --all-targets --all-features -- -D warnings

bold "tests (incl. invariant property tests)"
cargo test --all

if [[ "$QUICK" -eq 1 ]]; then
  printf '\n\033[1;32mQUICK GATE PASSED\033[0m\n'
  echo ""
  echo "Before merge or release, run the full gate:  ./scripts/gate.sh"
  exit 0
fi

bold "build (release workspace)"
cargo build --release --workspace
test -x "${CARGO_TARGET_DIR:-target}/release/demiurge-router"

bold "CPU bench gates (release hot paths)"
cargo run --release -q --package xtask -- bench-gate

bold "load regression smoke (CI scenarios)"
cargo run --release -q --package xtask -- load-bench --ci

bold "Track A fleet pilot (shadow π* + corrector shadow)"
cargo run --release -q --package xtask -- fleet-pilot

if [[ "$(uname -s)" == "Linux" ]]; then
  bold "Track B gate (required on Linux — BPF + XDP veth smoke)"
  ./scripts/track-b-gate.sh
else
  echo "skip Track B gate — macOS (see Track B below)"
fi

bold "spec build (optional)"
if command -v latexmk >/dev/null 2>&1; then
  ( cd spec && latexmk -pdf -interaction=nonstopmode -halt-on-error demiurge.tex >/dev/null )
  echo "spec compiled -> spec/demiurge.pdf"
else
  echo "latexmk not found; skipping spec build (CI builds it)"
fi

printf '\n\033[1;32mALL GATES PASSED\033[0m\n'
echo ""
echo "Optional Track A total verification (full metrics + soft spots, ~5 min):"
echo "  ./scripts/track-a-verify.sh  →  target/track-a-verify/report.md"
if [[ "$(uname -s)" == "Linux" ]]; then
  echo "Track B verification (gate + bench-probe + load + stress on Linux):"
  echo "  ./scripts/track-b-verify.sh           →  target/track-b-verify/report.md"
  echo "  ./scripts/track-b-verify.sh --quick   →  gate + CPU benches only"
  echo "  ./scripts/track-b-bench.sh            →  CPU probe/gate + XDP smoke"
else
  echo "Track B on macOS (Docker CI mirror):"
  echo "  ./scripts/linux-vm/docker-track-b.sh gate"
fi
