#!/usr/bin/env bash
# Local gate — mirrors the CI design-conformance + quality checks.
# Run before pushing; also wired as a pre-push hook by scripts/bootstrap.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

bold "regenerate artifacts from canonical inputs"
cargo xtask gen

bold "drift check (generated files must match canonical inputs)"
if ! git diff --quiet -- spec/generated crates/demiurge-cost/src/generated_params.rs; then
  echo "ERROR: generated artifacts are stale — run 'cargo xtask gen' and commit:" >&2
  git --no-pager diff --stat -- spec/generated crates/demiurge-cost/src/generated_params.rs >&2
  exit 1
fi

bold "traceability lint (spec <-> code <-> test)"
cargo xtask lint

bold "format check"
cargo fmt --all -- --check

bold "build (release workspace)"
cargo build --release --workspace
test -x target/release/demiurge-router

bold "clippy (warnings are errors)"
cargo clippy --all-targets --all-features -- -D warnings

bold "tests (incl. invariant property tests)"
cargo test --all

bold "CPU bench gates (release hot paths)"
cargo run --release -q --package xtask -- bench-gate

bold "load regression smoke (CI scenarios)"
cargo run --release -q --package xtask -- load-bench --ci

bold "Track A fleet pilot (shadow π* + corrector shadow)"
cargo run --release -q --package xtask -- fleet-pilot

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
