#!/usr/bin/env bash
# Stamp docs/PRODUCT-AND-DESIGN.md with release metadata + validation snapshot.
#
# Usage:
#   ARTIFACT_DIR=target/release-artifacts/.../staging \
#     ./scripts/generate-product-doc.sh [output.md]
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-${ARTIFACT_DIR:?ARTIFACT_DIR or output path required}/docs/PRODUCT-AND-DESIGN.md}"
SOURCE="$ROOT/docs/PRODUCT-AND-DESIGN.md"

VERSION="$(grep '^version' "$ROOT/crates/demiurge-router/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')"
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD)"
COMMIT_FULL="$(git -C "$ROOT" rev-parse HEAD)"
DATE="$(date -u +%Y-%m-%d)"
ARCH="$(uname -s)-$(uname -m)"
ARTIFACT_ROOT="$(dirname "$(dirname "$OUT")")"

BENCH_GATE="$ARTIFACT_ROOT/validation/bench-gate.log"
PRE_RELEASE="$ARTIFACT_ROOT/validation/pre-release.log"
LOAD_JSON="$ARTIFACT_ROOT/load-bench/load-full.json"
LOAD_JSON_FALLBACK="$ARTIFACT_ROOT/load-bench/latest.json"
STRESS_JSON="$ARTIFACT_ROOT/load-bench/stress.json"

LINT_OUT="$(mktemp)"
if cargo run --release -q --manifest-path "$ROOT/xtask/Cargo.toml" -- lint 2>&1 | tee "$LINT_OUT"; then
  LINT_STATUS="OK"
else
  LINT_STATUS="FAILED"
fi
LINT_SUMMARY="$(grep -E '^lint: OK|^lint: phase burndown' "$LINT_OUT" | paste -sd ' · ' - || echo "lint unavailable")"
rm -f "$LINT_OUT"

python3 - "$OUT" "$SOURCE" "$VERSION" "$COMMIT" "$COMMIT_FULL" "$DATE" "$ARCH" \
  "$LINT_STATUS" "$LINT_SUMMARY" "$PRE_RELEASE" "$BENCH_GATE" \
  "$LOAD_JSON" "$LOAD_JSON_FALLBACK" "$STRESS_JSON" <<'PY'
import re, sys
from pathlib import Path

(
    out,
    source,
    version,
    commit,
    commit_full,
    date,
    arch,
    lint_status,
    lint_summary,
    pre_release,
    bench_gate,
    load_json,
    load_fallback,
    stress_json,
) = sys.argv[1:]

def parse_bench_gate(path: Path) -> list[tuple[str, str, str]]:
    rows = []
    if not path.is_file():
        return rows
    for line in path.read_text().splitlines():
        m = re.search(
            r"bench-gate: (BENCH-\S+) OK — median (\d+) ns/op \(floor \d+, p95 \d+, limit (\d+) ns\)",
            line,
        )
        if m:
            rows.append((m.group(1), m.group(2), m.group(5)))
    return rows

def sum_scenarios(path: Path) -> tuple[int, int, int]:
    if not path.is_file():
        return 0, 0, 0
    import json

    data = json.loads(path.read_text())
    ok = err = total = 0
    for s in data.get("scenarios", []):
        ok += int(s.get("ok", 0))
        err += int(s.get("errors", 0))
        total += int(s.get("total_requests", 0))
    return ok, err, total

def load_totals(primary: Path, fallback: Path) -> tuple[int, int, int, str]:
    for label, path in (("load-full.json", primary), ("latest.json", fallback)):
        ok, err, total = sum_scenarios(path)
        if total > 0:
            return ok, err, total, label
    return 0, 0, 0, "none"

pre_status = "UNKNOWN"
pre_path = Path(pre_release)
if pre_path.is_file():
    text = pre_path.read_text()
    if "PRE-RELEASE PASSED" in text:
        pre_status = "PASSED"
    elif re.search(r"ERROR: pre-release failed|strict gate\(s\) failed", text):
        pre_status = "FAILED"

gate_rows = parse_bench_gate(Path(bench_gate))
load_ok, load_err, load_total, load_src = load_totals(Path(load_json), Path(load_fallback))
stress_ok, stress_err, stress_total = sum_scenarios(Path(stress_json))

body = Path(source).read_text()
# Drop the source-only footer line; release footer appended below.
body = re.sub(
    r"\n---\n\n\*Demiurge — design spec.*human brief.*\*\n?\s*$",
    "\n",
    body,
    flags=re.DOTALL,
)

header = f"""# Demiurge — Product & Technical Design

**Release build:** `{version}` · **commit** `{commit}` · **built** {date} UTC · **host** `{arch}`

> Stamped at publish time from [`docs/PRODUCT-AND-DESIGN.md`](https://github.com/fxdv/demiurge/blob/main/docs/PRODUCT-AND-DESIGN.md).
> Pair with `RELEASE-one-pager.md` in this artifact for raw validation logs.

## Validation snapshot (this build)

| Check | Result |
|-------|--------|
| Pre-release | **{pre_status}** |
| Traceability lint | **{lint_status}** — {lint_summary} |
| CPU bench gates | **{len(gate_rows)}** recorded |
| Load bench (`{load_src}`) | **{load_ok}/{load_total}** ok ({load_err} errors) |
| Stress (`stress.json`) | **{stress_ok}/{stress_total}** ok ({stress_err} errors) |

"""

if gate_rows:
    header += "| Gate | Median | Limit |\n|------|--------|-------|\n"
    for gate, median, limit in gate_rows[:10]:
        header += f"| `{gate}` | {median} ns | {limit} ns |\n"
    header += "\n"

header += "---\n\n"

# Skip duplicate H1 from source body.
body = re.sub(r"^# Demiurge — Product & Technical Design\s*\n", "", body, count=1)

footer = f"""
---

*Release artifact · `{version}` · `{commit_full}` · generated {date} UTC · `{arch}`*
"""

Path(out).parent.mkdir(parents=True, exist_ok=True)
Path(out).write_text(header + body + footer)
print(f"generate-product-doc: wrote {out}")
PY
