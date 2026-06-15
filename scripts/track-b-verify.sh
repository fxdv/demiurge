#!/usr/bin/env bash
# Track B total verification — toolchain, full gate, artifact report.
# Writes artifacts under target/track-b-verify/.
set -euo pipefail
cd "$(dirname "$0")/.."

ROOT="$PWD"
OUT="$ROOT/target/track-b-verify"
VAL="$OUT/validation"
mkdir -p "$VAL"

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
stamp() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "track-b-verify: Linux only (Track B VM or Linux CI)" >&2
  exit 1
fi

STARTED="$(stamp)"
bold "Track B verification started $STARTED"

bash ./scripts/ensure-rust-toolchain.sh 2>&1 | tee "$VAL/ensure-rust.log"

bold "full gate (includes Track B gate on Linux)"
./scripts/gate.sh 2>&1 | tee "$VAL/gate.log"

FINISHED="$(stamp)"
bold "generating observability report"
python3 - "$OUT" "$STARTED" "$FINISHED" <<'PY'
import json, sys
from pathlib import Path

out = Path(sys.argv[1])
started, finished = sys.argv[2], sys.argv[3]
val = out / "validation"

gate_log = (val / "gate.log").read_text() if (val / "gate.log").is_file() else ""
track_b_pass = "TRACK B GATE: PASSED" in gate_log and "ALL GATES PASSED" in gate_log

summary = {
    "started_utc": started,
    "finished_utc": finished,
    "track_b_gate_pass": track_b_pass,
    "bpf_object": str(out.parent.parent / "target" / "bpf" / "admit_shed.o"),
}
(out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

md = [
    "# Track B verification report",
    "",
    f"- **Started:** {started} UTC",
    f"- **Finished:** {finished} UTC",
    f"- **Gate + Track B:** {'PASS' if track_b_pass else 'FAIL'}",
    "",
    "## Artifacts",
    "",
    "- `validation/ensure-rust.log`",
    "- `validation/gate.log`",
    "- `summary.json`",
]
(out / "report.md").write_text("\n".join(md) + "\n")
print(f"report: {out / 'report.md'}")
PY

bold "report ready"
echo "  $OUT/report.md"
echo "  $OUT/summary.json"

if ! grep -q "ALL GATES PASSED" "$VAL/gate.log"; then
  printf '\n\033[1;31mTRACK B VERIFY: FAILED\033[0m\n' >&2
  exit 1
fi

printf '\n\033[1;32mTRACK B VERIFY: PASSED\033[0m\n'
