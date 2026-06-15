#!/usr/bin/env bash
# Track B total verification — gate, CPU benches, load + stress (Linux only).
# Writes artifacts under target/track-b-verify/.
#
#   ./scripts/track-b-verify.sh           # full (~5–10 min): gate + probe + load + stress
#   ./scripts/track-b-verify.sh --quick   # gate + bench-probe + p5 tests (no load/stress)
#   ./scripts/track-b-bench.sh            # CPU probe + bench-gate + Track B gate only
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=lib/ui.sh
source "$(dirname "$0")/lib/ui.sh"

ROOT="$PWD"
OUT="$ROOT/target/track-b-verify"
VAL="$OUT/validation"
mkdir -p "$VAL" "$OUT/load" "$OUT/stress"

stamp() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }

QUICK=0
for arg in "$@"; do
  case "$arg" in
    --quick) QUICK=1 ;;
    -h | --help)
      sed -n '1,12p' "$0"
      exit 0
      ;;
  esac
done

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "track-b-verify: Linux only (Track B VM or Linux CI)" >&2
  exit 1
fi

STARTED="$(stamp)"
if [[ "$QUICK" -eq 1 ]]; then
  demiurge_banner "DEMIURGE · Track B verification" \
    "mode    quick · gate + CPU benches" \
    "repo    $(_ui_git_ref)" \
    "host    $(_ui_host_tag) · linux"
else
  demiurge_banner "DEMIURGE · Track B verification" \
    "mode    full · gate + load + stress" \
    "repo    $(_ui_git_ref)" \
    "host    $(_ui_host_tag) · linux" \
    "note    proof ≠ production · mock TCP backends"
fi

bold "Track B verification started $STARTED (quick=$QUICK)"

bash ./scripts/ensure-rust-toolchain.sh 2>&1 | tee "$VAL/ensure-rust.log"

bold "full gate (CI mirror + required Track B gate)"
./scripts/gate.sh 2>&1 | tee "$VAL/gate.log"

bold "CPU bench-probe (headroom — watch BENCH-IOURING-FWD, BENCH-RCU-SNAPSHOT)"
cargo run --release -q --package xtask -- bench-probe 2>&1 | tee "$VAL/bench-probe.log"

bold "Track B router integration (p5 dataplane)"
cargo test -p demiurge-router --test p5_dataplane 2>&1 | tee "$VAL/p5-dataplane.log"

load_rc=0
stress_rc=0
if [[ "$QUICK" -eq 0 ]]; then
  bold "full load bench (router TCP + control metrics; incl. LOAD-STEP-ACTUATE)"
  set +e
  ./scripts/load-bench.sh 2>&1 | tee "$VAL/load-bench.log"
  load_rc=$?
  set -e
  cp -f target/load-bench/latest.json "$OUT/load/latest.json" 2>/dev/null || true
  cp -f target/load-bench/latest.pseudo "$OUT/load/latest.pseudo" 2>/dev/null || true

  bold "stress suite (strict zero-error)"
  set +e
  ./scripts/load-stress.sh 2>&1 | tee "$VAL/load-stress.log"
  stress_rc=$?
  set -e
  cp -f target/load-bench/stress.json "$OUT/stress/stress.json" 2>/dev/null || true
  cp -f target/load-bench/stress.pseudo "$OUT/stress/stress.pseudo" 2>/dev/null || true
else
  echo "skip load/stress (--quick)" | tee "$VAL/load-bench.log"
fi

FINISHED="$(stamp)"
bold "generating observability report"
python3 - "$OUT" "$STARTED" "$FINISHED" "$QUICK" "$load_rc" "$stress_rc" <<'PY'
import json, re, sys
from pathlib import Path

out = Path(sys.argv[1])
started, finished = sys.argv[2], sys.argv[3]
quick = int(sys.argv[4])
load_rc, stress_rc = int(sys.argv[5]), int(sys.argv[6])
val = out / "validation"

def read_log(name):
    p = val / name
    return p.read_text() if p.is_file() else ""

def parse_bench_probe(text):
    rows, thin = [], []
    for line in text.splitlines():
        if re.match(r"^BENCH-", line):
            parts = line.split()
            if len(parts) >= 7:
                rows.append({
                    "id": parts[0],
                    "floor_ns": parts[1],
                    "median_ns": parts[2],
                    "p95_ns": parts[3],
                    "limit_ns": parts[4],
                    "headroom": parts[5],
                    "thin": parts[6] == "YES",
                })
                if parts[6] == "YES":
                    thin.append(parts[0])
    return rows, thin

def load_json(path):
    p = Path(path)
    if not p.is_file():
        return None
    return json.loads(p.read_text())

gate_log = read_log("gate.log")
gate_pass = "ALL GATES PASSED" in gate_log
track_b_gate_pass = "TRACK B GATE: PASSED" in gate_log
probe_rows, thin_gates = parse_bench_probe(read_log("bench-probe.log"))
track_b_cpu = [r for r in probe_rows if r["id"] in ("BENCH-IOURING-FWD", "BENCH-RCU-SNAPSHOT")]

load = load_json(out / "load" / "latest.json") or {"scenarios": []}
stress = load_json(out / "stress" / "stress.json") or {"scenarios": []}

def scenario_table(scenarios, title):
    lines = [
        f"### {title}",
        "",
        "| scenario | ok/err | rps | p99 ms | π* | soft |",
        "|---|---:|---:|---:|---:|---|",
    ]
    soft = []
    for s in scenarios:
        p99_ms = s.get("p99_us", 0) / 1000.0
        max_p99 = s.get("max_p99_ms")
        gate = "·"
        if max_p99 is not None and s.get("ok", 0) > 0:
            gate = "OK" if p99_ms <= max_p99 else "FAIL"
            if p99_ms > max_p99 * 0.85:
                soft.append(f"{s['id']}: p99 {p99_ms:.1f}ms near limit {max_p99}ms")
        pi = s.get("pi_star")
        pi_s = f"{pi:.3f}" if pi is not None else "—"
        lines.append(
            f"| {s['id']} | {s.get('ok',0)}/{s.get('errors',0)} | "
            f"{s.get('req_per_sec',0):.0f} | {p99_ms:.1f} | {pi_s} | {gate} |"
        )
    lines.append("")
    return lines, soft

load_lines, load_soft = scenario_table(load.get("scenarios", []), "Load bench scenarios")
stress_lines, stress_soft = scenario_table(stress.get("scenarios", []), "Stress scenarios")

summary = {
    "started_utc": started,
    "finished_utc": finished,
    "quick_mode": bool(quick),
    "gate_pass": gate_pass,
    "track_b_gate_pass": track_b_gate_pass,
    "load_bench_rc": load_rc,
    "stress_rc": stress_rc,
    "thin_cpu_gates": thin_gates,
    "track_b_cpu_gates": track_b_cpu,
    "load_soft_spots": load_soft,
    "stress_soft_spots": stress_soft,
    "bpf_object": str(out.parent.parent / "target" / "bpf" / "admit_shed.o"),
}
(out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

md = [
    "# Track B verification report",
    "",
    f"- **Started:** {started} UTC",
    f"- **Finished:** {finished} UTC",
    f"- **Mode:** {'quick (no load/stress)' if quick else 'full'}",
    f"- **Artifacts:** `{out}`",
    "",
    "## Exit status",
    "",
    "| Stage | Result |",
    "|-------|--------|",
    f"| gate + Track B gate | {'PASS' if gate_pass and track_b_gate_pass else 'FAIL'} |",
    f"| load-bench (full) | {'SKIP' if quick else ('PASS' if load_rc == 0 else 'FAIL')} |",
    f"| stress (strict) | {'SKIP' if quick else ('PASS' if stress_rc == 0 else 'FAIL')} |",
    "",
    "## Track B CPU gates (bench-probe)",
    "",
]
if track_b_cpu:
    md.extend(["| gate | median | p95 | limit | headroom | thin |", "|---|---:|---:|---:|---:|:---:|"])
    for r in track_b_cpu:
        md.append(
            f"| {r['id']} | {r['median_ns']} | {r['p95_ns']} | {r['limit_ns']} | "
            f"{r['headroom']} | {'YES' if r['thin'] else '·'} |"
        )
else:
    md.append("(see validation/bench-probe.log)")
md.extend(["", "## All CPU gates", ""])
if thin_gates:
    md.append(f"**Thin gates:** {', '.join(thin_gates)}")
else:
    md.append("**No thin gates** at local median limits.")
md.extend(["", "| gate | median | p95 | limit | headroom | thin |", "|---|---:|---:|---:|---:|:---:|"])
for r in probe_rows:
    md.append(
        f"| {r['id']} | {r['median_ns']} | {r['p95_ns']} | {r['limit_ns']} | "
        f"{r['headroom']} | {'YES' if r['thin'] else '·'} |"
    )
if not quick:
    md.extend(["", *load_lines, *stress_lines])
md.extend(["", "## Soft spots summary", ""])
all_soft = []
if thin_gates:
    all_soft.append(f"CPU thin gates: {', '.join(thin_gates)}")
all_soft.extend(load_soft)
all_soft.extend(stress_soft)
if not all_soft:
    md.append("No soft spots flagged — all gates comfortable.")
else:
    for s in all_soft:
        md.append(f"- {s}")
md.extend([
    "",
    "## Logs",
    "",
    "- `validation/gate.log` — CI mirror + Track B gate",
    "- `validation/bench-probe.log` — CPU headroom (BENCH-IOURING-FWD)",
    "- `validation/p5-dataplane.log` — router Track B tests",
    "- `validation/load-bench.log` — full load scenarios",
    "- `validation/load-stress.log` — strict stress",
    "- `load/latest.pseudo` / `stress/stress.pseudo` — latency histograms",
    "- `summary.json` — machine-readable rollup",
])
(out / "report.md").write_text("\n".join(md) + "\n")
print(f"report: {out / 'report.md'}")
print(f"summary: {out / 'summary.json'}")
PY

bold "report ready"
echo "  $OUT/report.md"
echo "  $OUT/summary.json"
if [[ "$QUICK" -eq 0 ]]; then
  echo "  $OUT/load/latest.pseudo"
  echo "  $OUT/stress/stress.pseudo"
fi

fail=0
if ! grep -q "ALL GATES PASSED" "$VAL/gate.log"; then
  fail=1
fi
if [[ "$QUICK" -eq 0 ]] && { [[ "$load_rc" -ne 0 ]] || [[ "$stress_rc" -ne 0 ]]; }; then
  fail=1
fi

if [[ "$fail" -ne 0 ]]; then
  demiurge_fail "TRACK B VERIFY: FAILED (gate/load/stress — see report)"
  exit 1
fi

demiurge_pass "TRACK B VERIFY: PASSED"
