#!/usr/bin/env bash
# Local gate — mirrors CI (conformance + quality + regression).
#
#   ./scripts/gate.sh              # full CI mirror (default; pre-push hook)
#   ./scripts/gate.sh --quick      # inner loop: gen, drift, lint, fmt, clippy, test
#   ./scripts/gate.sh --ci-quality # CI quality job (conformance + fmt/clippy/test/release)
#   ./scripts/gate.sh --ci-regression  # CI regression (bench, load, fleet-pilot, Track B)
#
# Set GATE_SKIP_RELEASE_BUILD=1 to skip release build in --ci-regression (artifact reuse).
set -euo pipefail
cd "$(dirname "$0")/.."

QUICK=0
CI_PHASE=""
GATE_SKIP_RELEASE_BUILD="${GATE_SKIP_RELEASE_BUILD:-0}"

for arg in "$@"; do
  case "$arg" in
    --quick) QUICK=1 ;;
    --ci-conformance) CI_PHASE=conformance ;;
    --ci-quality) CI_PHASE=quality ;;
    --ci-regression) CI_PHASE=regression ;;
    -h | --help)
      sed -n '1,12p' "$0"
      echo ""
      echo "  --quick            skip release build, bench gates, load smoke, fleet-pilot, Track B, spec PDF"
      echo "  --ci-conformance   gen + drift + lint (design-conformance)"
      echo "  --ci-quality       conformance + fmt + clippy + test + release build"
      echo "  --ci-regression    bench-gate + load smoke + fleet-pilot + Track B (Linux)"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg (try --help)" >&2
      exit 2
      ;;
  esac
done

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

run_conformance() {
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
}

run_quality() {
  bold "format check"
  cargo fmt --all -- --check

  bold "clippy (warnings are errors)"
  cargo clippy --all-targets --all-features -- -D warnings

  bold "tests (incl. invariant property tests)"
  cargo test --all

  bold "build (release workspace)"
  cargo build --release --workspace
  test -x "${CARGO_TARGET_DIR:-target}/release/demiurge-router"
  test -x "${CARGO_TARGET_DIR:-target}/release/xtask"
}

run_regression() {
  if [[ "$GATE_SKIP_RELEASE_BUILD" != "1" ]]; then
    bold "build (release workspace)"
    cargo build --release --workspace
  fi

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
}

run_spec_optional() {
  bold "spec build (optional)"
  if command -v latexmk >/dev/null 2>&1; then
    ( cd spec && latexmk -pdf -interaction=nonstopmode -halt-on-error demiurge.tex >/dev/null )
    echo "spec compiled -> spec/demiurge.pdf"
  else
    echo "latexmk not found; skipping spec build (CI builds it)"
  fi
}

print_footer() {
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
}

case "$CI_PHASE" in
  conformance)
    run_conformance
    printf '\n\033[1;32mCONFORMANCE PASSED\033[0m\n'
    exit 0
    ;;
  quality)
    run_conformance
    run_quality
    printf '\n\033[1;32mQUALITY PASSED\033[0m\n'
    exit 0
    ;;
  regression)
    run_regression
    printf '\n\033[1;32mREGRESSION PASSED\033[0m\n'
    exit 0
    ;;
esac

run_conformance
run_quality

if [[ "$QUICK" -eq 1 ]]; then
  printf '\n\033[1;32mQUICK GATE PASSED\033[0m\n'
  echo ""
  echo "Before merge or release, run the full gate:  ./scripts/gate.sh"
  exit 0
fi

GATE_SKIP_RELEASE_BUILD=1
run_regression
run_spec_optional
print_footer
