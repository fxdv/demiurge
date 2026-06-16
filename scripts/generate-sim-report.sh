#!/usr/bin/env bash
# Archive 'sim outputs under target/reports/ (+ optional PDF via pandoc).
#
#   ./scripts/generate-sim-report.sh              run 'sim then archive
#   ./scripts/generate-sim-report.sh --skip-run   archive existing sim.pseudo/sim.json
set -euo pipefail
cd "$(dirname "$0")/.."

SKIP_RUN=0
for arg in "$@"; do
  case "$arg" in
    --skip-run) SKIP_RUN=1 ;;
    -h | --help)
      sed -n '1,6p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg (try --skip-run)" >&2
      exit 2
      ;;
  esac
done

mkdir -p target/reports target/load-bench

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

if [ "$SKIP_RUN" -eq 0 ]; then
  bold "'sim fleet replay"
  ./scripts/apostrophe-sim.sh
fi

PSEUDO_SRC="target/load-bench/sim.pseudo"
JSON_SRC="target/load-bench/sim.json"
if [ ! -f "$PSEUDO_SRC" ]; then
  echo "generate-sim-report: missing $PSEUDO_SRC (run ./scripts/apostrophe-sim.sh first)" >&2
  exit 1
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
REPORT_DIR="target/reports/sim-${STAMP}"

mkdir -p "$REPORT_DIR"
cp "$PSEUDO_SRC" "$REPORT_DIR/sim.pseudo"
cp "$JSON_SRC" "$REPORT_DIR/sim.json" 2>/dev/null || true
ln -sfn "$REPORT_DIR/sim.pseudo" target/reports/sim-latest.pseudo
ln -sfn "$REPORT_DIR/sim.json" target/reports/sim-latest.json 2>/dev/null || true

if command -v pandoc >/dev/null 2>&1; then
  bold "PDF disclosure report"
  pandoc "$REPORT_DIR/sim.pseudo" -o "$REPORT_DIR/sim-disclosure.pdf" \
    -V geometry:margin=1in -V fontsize=10pt 2>/dev/null || \
    echo "pandoc PDF skipped (install pandoc for PDF output)"
  ln -sfn "$REPORT_DIR/sim-disclosure.pdf" target/reports/sim-latest.pdf 2>/dev/null || true
else
  echo "pandoc not found — pseudo report only at $REPORT_DIR/sim.pseudo"
fi

echo "generate-sim-report: wrote $REPORT_DIR"
