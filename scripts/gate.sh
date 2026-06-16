#!/usr/bin/env bash
# Local gate — mirrors CI Gate workflow (Verify + Track A + Track B; Spec optional).
#
#   ./scripts/gate.sh              # full CI mirror (default; pre-push hook)
#   ./scripts/gate.sh --quick      # inner loop: gen, drift, lint, fmt, clippy, test
#   ./scripts/gate.sh --ci-quality     # CI verify job (conformance + fmt/clippy/test/release)
#   ./scripts/gate.sh --ci-track-a     # CI Track A (bench, load smoke, fleet-pilot)
#   ./scripts/gate.sh --ci-track-b     # CI Track B (BPF, XDP veth, kernel load — Linux)
#   ./scripts/gate.sh --ci-regression  # Track A + Track B (local full gate; CI runs jobs in parallel)
#
# Set GATE_SKIP_RELEASE_BUILD=1 to skip release build in --ci-regression (artifact reuse).
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=lib/ui.sh
source "$(dirname "$0")/lib/ui.sh"

QUICK=0
CI_PHASE=""
GATE_SKIP_RELEASE_BUILD="${GATE_SKIP_RELEASE_BUILD:-0}"

for arg in "$@"; do
  case "$arg" in
    --quick) QUICK=1 ;;
    --ci-conformance) CI_PHASE=conformance ;;
    --ci-quality) CI_PHASE=quality ;;
    --ci-track-a) CI_PHASE=track-a ;;
    --ci-track-b) CI_PHASE=track-b ;;
    --ci-regression) CI_PHASE=regression ;;
    -h | --help)
      sed -n '1,12p' "$0"
      echo ""
      echo "  --quick            skip release build, bench gates, load smoke, fleet-pilot, Track B, spec PDF"
      echo "  --ci-conformance   gen + drift + lint (design-conformance)"
      echo "  --ci-quality       conformance + fmt + clippy + test + release build"
      echo "  --ci-track-a       bench-gate + load smoke + fleet-pilot"
      echo "  --ci-track-b       BPF + XDP veth + p5 tests + LOAD-TRACK-B-KERNEL (Linux)"
      echo "  --ci-regression    --ci-track-a then --ci-track-b (sequential; CI runs in parallel)"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg (try --help)" >&2
      exit 2
      ;;
  esac
done

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

ensure_release_build() {
  if [[ "$GATE_SKIP_RELEASE_BUILD" != "1" ]]; then
    bold "build (release workspace)"
    cargo build --release --workspace
  fi
}

run_track_a() {
  ensure_release_build

  bold "CPU bench gates (release hot paths)"
  cargo run --release -q --package xtask -- bench-gate

  bold "load regression smoke (CI scenarios)"
  cargo run --release -q --package xtask -- load-bench --ci

  bold "Track A fleet pilot (shadow π* + corrector shadow)"
  cargo run --release -q --package xtask -- fleet-pilot

  bold "fleet simulation integration (DEMI-FLEET-SIM)"
  cargo test -p demiurge-control --test fleet_sim_integration -q
}

run_track_b() {
  ensure_release_build

  if [[ "$(uname -s)" != "Linux" ]]; then
    echo "skip Track B gate — macOS (see Track B below)"
    return 0
  fi

  bold "Track B gate (BPF compile + XDP veth smoke)"
  ./scripts/track-b-gate.sh

  bold "router Track B dataplane tests"
  cargo test -p demiurge-router --test p5_dataplane

  bold "Track B kernel load (LOAD-TRACK-B-KERNEL)"
  as_root() {
    if [[ "$(id -u)" -eq 0 ]]; then
      "$@"
    else
      sudo -E env PATH="$PATH" "$@"
    fi
  }
  as_root env GATE_SKIP_RELEASE_BUILD=1 \
    cargo run --release -q --package xtask -- load-bench --scenario LOAD-TRACK-B-KERNEL
}

run_regression() {
  run_track_a
  run_track_b
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
  demiurge_pass "ALL GATES PASSED"
  echo ""
  echo "Optional verification suites (rerunnable):"
  echo "  ./scripts/verify.sh list              all suites + artifact paths"
  echo "  ./scripts/verify.sh harden            Tiers 1–4 die-hard (+ report.pseudo)"
  echo "  ./scripts/verify.sh full              gate + load + stress + harden + 'sim"
  echo "Optional Track A total verification (full metrics + soft spots, ~5 min):"
  echo "  ./scripts/verify.sh track-a           →  target/track-a-verify/report.md"
  if [[ "$(uname -s)" == "Linux" ]]; then
    echo "Track B verification (gate + bench-probe + load + stress on Linux):"
    echo "  ./scripts/verify.sh track-b             →  target/track-b-verify/report.md"
    echo "  ./scripts/verify.sh track-b --quick     →  gate + CPU benches only"
    echo "  ./scripts/track-b-bench.sh              →  CPU probe/gate + XDP smoke"
  else
    echo "Track B on macOS (Docker CI mirror):"
    echo "  ./scripts/linux-vm/docker-track-b.sh gate"
  fi
}

case "$CI_PHASE" in
  conformance)
    demiurge_banner "DEMIURGE · verification gate" \
      "mode    CI · conformance" \
      "repo    $(_ui_git_ref)" \
      "host    $(_ui_host_tag)"
    run_conformance
    demiurge_pass "CONFORMANCE PASSED"
    exit 0
    ;;
  quality)
    demiurge_banner "DEMIURGE · verification gate" \
      "mode    CI · quality" \
      "repo    $(_ui_git_ref)" \
      "host    $(_ui_host_tag)"
    run_conformance
    run_quality
    demiurge_pass "QUALITY PASSED"
    exit 0
    ;;
  track-a)
    demiurge_banner "DEMIURGE · verification gate" \
      "mode    CI · Track A" \
      "repo    $(_ui_git_ref)" \
      "host    $(_ui_host_tag)"
    run_track_a
    demiurge_pass "TRACK A PASSED"
    exit 0
    ;;
  track-b)
    demiurge_banner "DEMIURGE · verification gate" \
      "mode    CI · Track B" \
      "repo    $(_ui_git_ref)" \
      "host    $(_ui_host_tag)"
    run_track_b
    demiurge_pass "TRACK B PASSED"
    exit 0
    ;;
  regression)
    demiurge_banner "DEMIURGE · verification gate" \
      "mode    CI · regression (Track A + B)" \
      "repo    $(_ui_git_ref)" \
      "host    $(_ui_host_tag)"
    run_regression
    demiurge_pass "REGRESSION PASSED"
    exit 0
    ;;
esac

if [[ "$QUICK" -eq 0 ]]; then
  demiurge_banner "DEMIURGE · verification gate" \
    "mode    full · track A + B (platform-dependent)" \
    "repo    $(_ui_git_ref)" \
    "host    $(_ui_host_tag)"
fi

run_conformance
run_quality

if [[ "$QUICK" -eq 1 ]]; then
  demiurge_pass "QUICK GATE PASSED"
  echo ""
  echo "Before merge or release, run the full gate:  ./scripts/gate.sh"
  exit 0
fi

GATE_SKIP_RELEASE_BUILD=1
run_regression
run_spec_optional
print_footer
