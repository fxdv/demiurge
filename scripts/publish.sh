#!/usr/bin/env bash
# Build a release artifact: binaries + pre-release validation + technical one-pager.
#
# Usage:
#   ./scripts/publish.sh                    # full pre-release + pack
#   ./scripts/publish.sh --skip-pre-release # pack from last validation run
#   ./scripts/publish.sh --github v0.1.0-p5 # also tag + gh release upload
#
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

SKIP_PRE=false
GITHUB_TAG=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-pre-release) SKIP_PRE=true; shift ;;
    --github)
      GITHUB_TAG="${2:?--github requires a tag name, e.g. v0.1.0-p5}"
      shift 2
      ;;
    -h|--help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

VERSION="$(grep '^version' crates/demiurge-router/Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')"
COMMIT="$(git rev-parse --short HEAD)"
ARCH="$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)"
STAGING="target/release-artifacts/demiurge-${VERSION}-${ARCH}-${COMMIT}"
TARBALL="target/release-artifacts/demiurge-${VERSION}-${ARCH}-${COMMIT}.tar.gz"

rm -rf "$STAGING"
mkdir -p "$STAGING"/{bin,validation,load-bench,design}

VALIDATION="$STAGING/validation"
PRE_LOG="$VALIDATION/pre-release.log"

if [[ "$SKIP_PRE" == true ]]; then
  bold "skip pre-release (--skip-pre-release)"
  if [[ ! -d target/load-bench/runs ]] && [[ ! -f target/load-bench/latest.json ]]; then
    echo "ERROR: no load-bench output — run ./scripts/pre-release.sh first" >&2
    exit 1
  fi
else
  bold "pre-release validation"
  if ! bash ./scripts/pre-release.sh 2>&1 | tee "$PRE_LOG"; then
    echo "ERROR: pre-release failed — fix before publishing" >&2
    exit 1
  fi
fi

bold "release build (ensure fresh binaries)"
cargo build --release --workspace
test -x target/release/demiurge-router
test -x target/release/xtask

bold "capture bench logs"
cargo run --release -q --package xtask -- bench-gate 2>&1 | tee "$VALIDATION/bench-gate.log"
cargo run --release -q --package xtask -- bench-probe 2>&1 | tee "$VALIDATION/bench-probe.log"

bold "stage binaries"
cp target/release/demiurge-router target/release/xtask "$STAGING/bin/"
chmod +x "$STAGING/bin/"*

bold "stage load-bench reports"
merge_load_json() {
  python3 <<'PY'
import json
from datetime import datetime, timezone
from pathlib import Path

root = Path("target/load-bench")
runs = root / "runs"
scenarios = []
if runs.is_dir():
    for path in sorted(runs.glob("*.json")):
        if path.stem.startswith("LOAD-STRESS"):
            continue
        data = json.loads(path.read_text())
        scenarios.extend(data.get("scenarios", []))
if scenarios:
    report = {
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "hostname": "merged-for-publish",
        "scenarios": scenarios,
    }
    out = root / "latest.json"
    out.write_text(json.dumps(report, indent=2) + "\n")
    print(f"merge-load-reports: {len(scenarios)} scenario(s) → {out}")
else:
    print("merge-load-reports: no load runs/ — keeping existing latest.json")
PY
}
merge_load_json
cargo run --release -q --package xtask -- load-report 2>/dev/null || true
cp target/load-bench/latest.json target/load-bench/latest.pseudo "$STAGING/load-bench/"
if [[ -f target/load-bench/stress.json ]]; then
  cargo run --release -q --package xtask -- load-report --stress
  cp target/load-bench/stress.json target/load-bench/stress.pseudo "$STAGING/load-bench/"
fi
if [[ -d target/load-bench/runs ]]; then
  mkdir -p "$STAGING/load-bench/runs"
  cp target/load-bench/runs/*.json "$STAGING/load-bench/runs/" 2>/dev/null || true
fi
cp design/bench-gates.toml "$STAGING/design/"

if [[ -f target/release-artifacts/pre-release.log ]] && [[ ! -s "$PRE_LOG" ]]; then
  cp target/release-artifacts/pre-release.log "$PRE_LOG"
fi

bold "generate one-pager"
ARTIFACT_DIR="$STAGING" bash ./scripts/generate-one-pager.sh "$STAGING/RELEASE-one-pager.md"

bold "manifest + checksums"
{
  echo "Demiurge release artifact"
  echo "version=$VERSION commit=$COMMIT arch=$ARCH"
  echo "generated=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo
  find "$STAGING" -type f ! -name MANIFEST.txt | sort | while read -r f; do
    rel="${f#"$STAGING"/}"
    sum="$(shasum -a 256 "$f" | awk '{print $1}')"
    printf '%s  %s\n' "$sum" "$rel"
  done
} > "$STAGING/MANIFEST.txt"

mkdir -p target/release-artifacts
tar -czf "$TARBALL" -C "$(dirname "$STAGING")" "$(basename "$STAGING")"

{
  echo "VERSION=$VERSION"
  echo "COMMIT=$COMMIT"
  echo "ARCH=$ARCH"
  echo "STAGING=$STAGING"
  echo "TARBALL=$TARBALL"
} > target/release-artifacts/publish.env

bold "artifact ready"
echo "  dir:     $STAGING"
echo "  tarball: $TARBALL"
echo "  one-pager: $STAGING/RELEASE-one-pager.md"
ls -lh "$STAGING/bin/"* "$TARBALL"

if [[ -n "$GITHUB_TAG" ]]; then
  bold "GitHub release $GITHUB_TAG"
  if ! command -v gh >/dev/null 2>&1; then
    echo "ERROR: gh CLI not found" >&2
    exit 1
  fi
  git tag -a "$GITHUB_TAG" -m "Demiurge $VERSION ($COMMIT)" 2>/dev/null || true
  git push origin "$GITHUB_TAG"
  gh release create "$GITHUB_TAG" \
    --title "Demiurge ${VERSION} — P5 proof (${COMMIT})" \
    --notes-file "$STAGING/RELEASE-one-pager.md"
  bash ./scripts/gh-release-upload.sh "$GITHUB_TAG"
  echo "GitHub release published for $GITHUB_TAG"
fi

printf '\n\033[1;32mPUBLISH OK\033[0m\n'
