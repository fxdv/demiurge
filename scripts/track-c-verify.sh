#!/usr/bin/env bash
# Track C verification — portable P6/P7/P8 logic + live GPU fleet P/D proof.
# Writes artifacts under target/track-c-verify/.
#
#   ./scripts/track-c-verify.sh              # logic + live smoke + warmth bench
#   ./scripts/track-c-verify.sh --quick      # logic + live smoke (no warmth bench)
#   ./scripts/track-c-verify.sh --logic-only # P6/P7/P8 tests only (no vLLM)
#   ./scripts/track-c-verify.sh --ensure-up  # start vLLM + router before live checks
#
# Intended host: singularity reference fleet (Linux + 4× V100). Live stages
# require vLLM workers and demiurge-router on 127.0.0.1:8080.
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=lib/ui.sh
source "$(dirname "$0")/lib/ui.sh"

ROOT="$PWD"
OUT="$ROOT/target/track-c-verify"
VAL="$OUT/validation"
mkdir -p "$VAL"

stamp() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }

QUICK=0
LOGIC_ONLY=0
ENSURE_UP=0
for arg in "$@"; do
  case "$arg" in
    --quick) QUICK=1 ;;
    --logic-only) LOGIC_ONLY=1 ;;
    --ensure-up) ENSURE_UP=1 ;;
    -h | --help)
      sed -n '1,14p' "$0"
      exit 0
      ;;
  esac
done

if [[ "$(uname -s)" != "Linux" ]] && [[ "$LOGIC_ONLY" -eq 0 ]]; then
  echo "track-c-verify: live fleet checks require Linux (use --logic-only elsewhere)" >&2
  exit 1
fi

STARTED="$(stamp)"
MODE="full"
if [[ "$LOGIC_ONLY" -eq 1 ]]; then MODE="logic-only"; fi
if [[ "$QUICK" -eq 1 ]]; then MODE="quick"; fi

demiurge_banner "DEMIURGE · Track C verification" \
  "mode    $MODE · P6/P7/P8 logic + live P/D proof" \
  "repo    $(_ui_git_ref)" \
  "host    $(_ui_host_tag)" \
  "note    closes P/D proof gate · RDMA/migration-p99/corrector-live still open"

bold "Track C verification started $STARTED"

bash ./scripts/ensure-rust-toolchain.sh 2>&1 | tee "$VAL/ensure-rust.log"

bold "Phase 6 — migration logic (DEMI-MIG-SUBITL)"
cargo test -p demiurge-control migration 2>&1 | tee "$VAL/migration.log"

bold "Phase 7 — cache-domain isolation (DEMI-S1-DOMAIN)"
cargo test -p demiurge-auth 2>&1 | tee "$VAL/auth.log"
cargo test -p demiurge-router --test p7_cache_isolation 2>&1 | tee "$VAL/p7-cache-isolation.log"

bold "Phase 8 — corrector graduation (DEMI-CORR-GRAD)"
cargo test -p demiurge-control corrector_grad 2>&1 | tee "$VAL/corrector-grad.log"

bold "P/D stack — KV ledger, warmth, RDMA shadow"
cargo test -p demiurge-router --test p2_kv 2>&1 | tee "$VAL/p2-kv.log"
cargo test -p demiurge-router --test p3_warmth 2>&1 | tee "$VAL/p3-warmth.log"
cargo test -p demiurge-router --test rdma_shadow 2>&1 | tee "$VAL/rdma-shadow.log"

live_rc=0
warmth_rc=0
if [[ "$LOGIC_ONLY" -eq 0 ]]; then
  if [[ "$ENSURE_UP" -eq 1 ]]; then
    bold "ensure fleet up (vLLM + shims + router)"
    bash ./scripts/singularity/start-vllm-pd.sh 2>&1 | tee "$VAL/start-vllm-pd.log"
    bash ./scripts/singularity/start-router.sh 2>&1 | tee "$VAL/start-router.log"
  else
    bold "restart router (fresh state plane for colocated smoke)"
    bash ./scripts/singularity/start-router.sh 2>&1 | tee "$VAL/start-router.log"
    sleep 1
  fi

  bold "live smoke (models + colocated + disaggregated)"
  set +e
  python3 ./scripts/singularity/track-c-live-smoke.py 2>&1 | tee "$VAL/live-smoke.json"
  live_rc=$?
  set -e

  if [[ "$QUICK" -eq 0 ]]; then
    bold "warmth prefix-locality bench (C4 / TC-WARMTH-SKEW)"
    set +e
    python3 ./scripts/singularity/warmth-prefix-bench.py 2>&1 | tee "$VAL/warmth-bench.log"
    warmth_rc=$?
    if grep -q "PASS: warmth skew visible" "$VAL/warmth-bench.log"; then
      warmth_rc=0
    else
      warmth_rc=1
    fi
    set -e
  else
    echo "skip warmth bench (--quick)" | tee "$VAL/warmth-bench.log"
  fi
else
  echo "skip live fleet (--logic-only)" | tee "$VAL/live-smoke.json"
  echo "skip warmth bench (--logic-only)" | tee "$VAL/warmth-bench.log"
fi

FINISHED="$(stamp)"
bold "generating report"
python3 - "$OUT" "$STARTED" "$FINISHED" "$MODE" "$live_rc" "$warmth_rc" <<'PY'
import json, re, sys
from pathlib import Path

out = Path(sys.argv[1])
started, finished = sys.argv[2], sys.argv[3]
mode = sys.argv[4]
live_rc, warmth_rc = int(sys.argv[5]), int(sys.argv[6])
val = out / "validation"

def read_log(name):
    p = val / name
    return p.read_text() if p.is_file() else ""

def log_pass(name):
    text = read_log(name)
    return "test result: ok" in text.lower() or "running 0 tests" in text.lower()

logic_stages = [
    ("TC-MIG-UNIT", "migration.log"),
    ("TC-P7-UNIT", "p7-cache-isolation.log"),
    ("TC-P8-UNIT", "corrector-grad.log"),
    ("TC-KV-UNIT", "p2-kv.log"),
    ("TC-WARM-UNIT", "p3-warmth.log"),
    ("TC-RDMA-SHADOW", "rdma-shadow.log"),
]

live_pass = False
live_checks = []
smoke_path = val / "live-smoke.json"
if smoke_path.is_file():
    try:
        smoke = json.loads(smoke_path.read_text())
        live_pass = smoke.get("pass", False)
        live_checks = smoke.get("checks", [])
    except json.JSONDecodeError:
        live_pass = False

warmth_pass = "PASS: warmth skew visible" in read_log("warmth-bench.log")
warmth_skipped = mode in ("logic-only", "quick")

open_gates = [
    "RDMA production KV handoff (TCP proof only today)",
    "Migration cutover p99 measured on fleet (DEMI-MIG-SUBITL fleet gate)",
    "Corrector graduation wired to live traffic (δ=1 in prod router)",
    "Real tenant auth/content verification on production traffic",
    "Pool actuation at GPU fleet scale (shadow → canary → prod)",
]

summary = {
    "started_utc": started,
    "finished_utc": finished,
    "mode": mode,
    "logic_stages": {sid: log_pass(log) for sid, log in logic_stages},
    "live_smoke_pass": live_pass if mode != "logic-only" else None,
    "warmth_skew_pass": warmth_pass if not warmth_skipped else None,
    "live_rc": live_rc,
    "warmth_rc": warmth_rc,
    "open_roadmap_gates": open_gates,
}
logic_all = all(summary["logic_stages"].values())
gate_pass = logic_all
if mode != "logic-only":
    gate_pass = gate_pass and live_pass and (warmth_pass or warmth_skipped)
summary["gate_pass"] = gate_pass
(out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

md = [
    "# Track C verification report",
    "",
    f"- **Started:** {started} UTC",
    f"- **Finished:** {finished} UTC",
    f"- **Mode:** {mode}",
    f"- **Artifacts:** `{out}`",
    "",
    "## Exit status",
    "",
    "| Stage | ID | Result |",
    "|-------|-----|--------|",
]
for sid, log in logic_stages:
    md.append(f"| Logic | {sid} | {'PASS' if summary['logic_stages'][sid] else 'FAIL'} |")
if mode == "logic-only":
    md.append("| Live | TC-LIVE-* | SKIP |")
    md.append("| Live | TC-WARMTH-SKEW | SKIP |")
else:
    md.append(f"| Live | TC-LIVE-SMOKE | {'PASS' if live_pass else 'FAIL'} |")
    if mode == "quick":
        md.append("| Live | TC-WARMTH-SKEW | SKIP (--quick) |")
    else:
        md.append(f"| Live | TC-WARMTH-SKEW | {'PASS' if warmth_pass else 'FAIL'} |")
md.extend([
    "",
    f"**Track C P/D proof gate:** {'PASS' if gate_pass else 'FAIL'}",
    "",
    "## Live smoke detail",
    "",
])
if live_checks:
    md.extend(["| check | HTTP | latency ms | pass |", "|---|---:|---:|:---:|"])
    for c in live_checks:
        lat = c.get("latency_ms", "—")
        md.append(f"| {c['id']} | {c.get('http_code', '—')} | {lat} | {'✓' if c['pass'] else '✗'} |")
else:
    md.append("(no live checks in this mode)")
md.extend([
    "",
    "## Roadmap gates still open (full Track C closure)",
    "",
])
for g in open_gates:
    md.append(f"- {g}")
md.extend([
    "",
    "## Logs",
    "",
    "- `validation/migration.log` — Phase 6 unit tests",
    "- `validation/p7-cache-isolation.log` — Phase 7 router wiring",
    "- `validation/corrector-grad.log` — Phase 8 graduation FSM",
    "- `validation/live-smoke.json` — models + colocated + disagg",
    "- `validation/warmth-bench.log` — prefix-locality skew",
    "- `summary.json` — machine-readable rollup",
])
(out / "report.md").write_text("\n".join(md) + "\n")
print(f"report: {out / 'report.md'}")
print(f"summary: {out / 'summary.json'}")
PY

bold "report ready"
echo "  $OUT/report.md"
echo "  $OUT/summary.json"

fail=0
if ! python3 - <<PY
import json, sys
s = json.load(open("$OUT/summary.json"))
sys.exit(0 if s.get("gate_pass") else 1)
PY
then
  fail=1
fi

if [[ "$fail" -ne 0 ]]; then
  demiurge_fail "TRACK C VERIFY: FAILED (see $OUT/report.md)"
  exit 1
fi

demiurge_pass "TRACK C VERIFY: PASSED (P/D proof gate — see report for open roadmap items)"
