#!/usr/bin/env bash
# Track A total verification — metrics, soft spots, full load + stress observability.
# Writes artifacts under target/track-a-verify/ and a consolidated report.md.
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=lib/ui.sh
source "$(dirname "$0")/lib/ui.sh"

ROOT="$PWD"
OUT="$ROOT/target/track-a-verify"
VAL="$OUT/validation"
mkdir -p "$VAL" "$OUT/load" "$OUT/stress" "$OUT/fleet-pilot"

stamp() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }

STARTED="$(stamp)"
demiurge_banner "DEMIURGE · Track A verification" \
  "mode    full · metrics + load + stress" \
  "repo    $(_ui_git_ref)" \
  "host    $(_ui_host_tag)" \
  "note    proof ≠ production · mock TCP backends"

bold "Track A verification started $STARTED"

bold "lint + phase burndown"
cargo xtask gen >/dev/null
cargo xtask lint 2>&1 | tee "$VAL/lint.log"

bold "unit + integration tests (all crates)"
cargo test --all 2>&1 | tee "$VAL/test-all.log"

bold "Track A integration tests"
cargo test -p demiurge-router --test track_a 2>&1 | tee "$VAL/track_a.log"

bold "CPU bench-probe (soft spots / headroom)"
cargo run --release -q --package xtask -- bench-probe 2>&1 | tee "$VAL/bench-probe.log"

bold "CPU bench-gate (release hot paths)"
cargo run --release -q --package xtask -- bench-gate 2>&1 | tee "$VAL/bench-gate.log"

bold "CPU gate flame (hierarchy + headroom trends)"
cargo run --release -q --package xtask -- bench-flame 2>&1 | tee "$VAL/bench-flame.log"
cp -f target/bench-probe/flame.svg "$OUT/flame.svg" 2>/dev/null || true

bold "fleet-pilot (shadow π* + corrector shadow)"
cargo run --release -q --package xtask -- fleet-pilot 2>&1 | tee "$VAL/fleet-pilot.log"
cp -f target/fleet-pilot/latest.json "$OUT/fleet-pilot/latest.json"

bold "full load bench (all scenarios + control metrics)"
set +e
cargo run --release -q --package xtask -- load-bench 2>&1 | tee "$VAL/load-bench.log"
load_rc=$?
set -e
cargo run --release -q --package xtask -- load-report 2>&1 | tee "$VAL/load-report.log"
cp -f target/load-bench/latest.json "$OUT/load/latest.json"
cp -f target/load-bench/latest.pseudo "$OUT/load/latest.pseudo"

bold "stress suite (strict zero-error)"
sleep 30
set +e
cargo run --release -q --package xtask -- load-bench --stress 2>&1 | tee "$VAL/load-stress.log"
stress_rc=$?
set -e
cargo run --release -q --package xtask -- load-report --stress 2>&1 | tee "$VAL/stress-report.log"
cp -f target/load-bench/stress.json "$OUT/stress/stress.json" 2>/dev/null || true
cp -f target/load-bench/stress.pseudo "$OUT/stress/stress.pseudo" 2>/dev/null || true

FINISHED="$(stamp)"
bold "generating observability report"
python3 - "$OUT" "$STARTED" "$FINISHED" "$load_rc" "$stress_rc" <<'PY'
import json, re, sys
from pathlib import Path

out = Path(sys.argv[1])
started, finished = sys.argv[2], sys.argv[3]
load_rc, stress_rc = int(sys.argv[4]), int(sys.argv[5])
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

lint = read_log("lint.log")
burndown = [l.strip() for l in lint.splitlines() if l.startswith("lint: phase burndown")]
probe_rows, thin_gates = parse_bench_probe(read_log("bench-probe.log"))

load = load_json(out / "load" / "latest.json") or {"scenarios": []}
stress = load_json(out / "stress" / "stress.json") or {"scenarios": []}
fleet = load_json(out / "fleet-pilot" / "latest.json") or {}

def scenario_table(scenarios, title):
    lines = [f"### {title}", "", "| scenario | ok/err | rps | p99 ms | fast_path | π* | misroute n | shadow n | soft |", "|---|---:|---:|---:|---:|---:|---:|---:|---|"]
    soft = []
    for s in scenarios:
        p99_ms = s.get("p99_us", 0) / 1000.0
        max_p99 = s.get("max_p99_ms")
        gate = "·"
        if max_p99 is not None and s.get("ok", 0) > 0:
            gate = "OK" if p99_ms <= max_p99 else "FAIL"
            if p99_ms > max_p99 * 0.85:
                soft.append(f"{s['id']}: p99 {p99_ms:.1f}ms near limit {max_p99}ms")
        fp = s.get("fast_path_ratio")
        fp_s = f"{fp:.3f}" if fp is not None else "—"
        pi = s.get("pi_star")
        pi_s = f"{pi:.3f}" if pi is not None else "—"
        mr = s.get("fastpath_misroute_samples") or 0
        cs = s.get("corrector_shadow_samples") or 0
        lines.append(
            f"| {s['id']} | {s.get('ok',0)}/{s.get('errors',0)} | {s.get('req_per_sec',0):.0f} | {p99_ms:.1f} | {fp_s} | {pi_s} | {mr} | {cs} | {gate} |"
        )
    lines.append("")
    return lines, soft

load_lines, load_soft = scenario_table(load.get("scenarios", []), "Load bench scenarios")
stress_lines, stress_soft = scenario_table(stress.get("scenarios", []), "Stress scenarios")

fp = fleet.get("fleet_pilot", {})
cs = fleet.get("corrector_shadow", {})
corr = fp.get("heldout_correlation")
corr_margin = (corr - 0.45) if isinstance(corr, (int, float)) else None

summary = {
    "started_utc": started,
    "finished_utc": finished,
    "load_bench_rc": load_rc,
    "stress_rc": stress_rc,
    "thin_cpu_gates": thin_gates,
    "fleet_pilot": fp,
    "corrector_shadow": cs,
    "load_soft_spots": load_soft,
    "stress_soft_spots": stress_soft,
}
(out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

md = [
    "# Track A verification report",
    "",
    f"- **Started:** {started} UTC",
    f"- **Finished:** {finished} UTC",
    f"- **Artifacts:** `{out.relative_to(out.parent.parent)}`",
    "",
    "## Exit status",
    "",
    f"| Stage | Result |",
    f"|-------|--------|",
    f"| load-bench (full) | {'PASS' if load_rc == 0 else 'FAIL'} |",
    f"| stress (strict) | {'PASS' if stress_rc == 0 else 'FAIL'} |",
    f"| fleet-pilot gate | {'PASS' if fp.get('gate_pass') else 'FAIL'} |",
    "",
    "## Phase burndown",
    "",
    "```",
    burndown[0] if burndown else "(see validation/lint.log)",
    "```",
    "",
    "## CPU bench — soft spots (bench-probe)",
    "",
]
if thin_gates:
    md.append(f"**Thin gates:** {', '.join(thin_gates)}")
else:
    md.append("**No thin gates** at local median limits.")
md.extend(["", "| gate | median | p95 | limit | headroom | thin |", "|---|---:|---:|---:|---:|:---:|"])
for r in probe_rows:
    md.append(f"| {r['id']} | {r['median_ns']} | {r['p95_ns']} | {r['limit_ns']} | {r['headroom']} | {'YES' if r['thin'] else '·'} |")
if (out / "flame.svg").is_file():
    md.extend(["", "![CPU gate flame — call hierarchy, headroom heat, median trends](flame.svg)"])
md.extend(["", "## Fleet pilot (shadow)", ""])
if fp:
    md.extend([
        f"- held-out correlation: **{fp.get('heldout_correlation', 0):.3f}** (min 0.45, margin {corr_margin:.3f})" if corr_margin is not None else "",
        f"- π* heavy / light: **{fp.get('heldout_mean_pi_heavy', 0):.3f}** / **{fp.get('heldout_mean_pi_light', 0):.3f}**",
        f"- windows: train={fp.get('train_windows', 0)} held-out={fp.get('heldout_windows', 0)}",
    ])
if cs:
    md.extend([
        "",
        "## Corrector shadow (offline gate)",
        "",
        f"- trained δ: **{cs.get('delta', 0):.4f}**",
        f"- goodput improvement: **{cs.get('goodput_improvement', 0)*100:.1f}%** (min 25%)",
    ])
md.extend(["", *load_lines, *stress_lines, "## Soft spots summary", ""])
all_soft = []
if thin_gates:
    all_soft.append(f"CPU thin gates: {', '.join(thin_gates)}")
all_soft.extend(load_soft)
all_soft.extend(stress_soft)
if corr_margin is not None and corr_margin < 0.15:
    all_soft.append(f"fleet-pilot: held-out corr margin only {corr_margin:.3f} above 0.45 floor")
if not all_soft:
    md.append("No soft spots flagged — all gates comfortable.")
else:
    for s in all_soft:
        md.append(f"- {s}")
md.extend([
    "",
    "## Logs",
    "",
    "- `validation/bench-probe.log` — CPU headroom",
    "- `flame.svg` — CPU gate flame (call hierarchy + headroom trends)",
    "- `validation/fleet-pilot.log` — π* + corrector shadow",
    "- `validation/load-bench.log` — full scenario run",
    "- `load/latest.pseudo` — latency histograms",
    "- `stress/stress.pseudo` — strict stress report",
    "- `summary.json` — machine-readable rollup",
])
(out / "report.md").write_text("\n".join(md) + "\n")
print(f"report: {out / 'report.md'}")
print(f"summary: {out / 'summary.json'}")
PY

bold "report ready"
echo "  $OUT/report.md"
echo "  $OUT/summary.json"
echo "  $OUT/flame.svg"
echo "  $OUT/load/latest.pseudo"
echo "  $OUT/stress/stress.pseudo"

if [ "$load_rc" -ne 0 ] || [ "$stress_rc" -ne 0 ]; then
  demiurge_fail "TRACK A VERIFY: FAILED (load_rc=$load_rc stress_rc=$stress_rc)"
  exit 1
fi

demiurge_pass "TRACK A VERIFY: PASSED"
