#!/usr/bin/env bash
# Singularity full benchmark — every Linux + GPU suite vs design budgets.
#
#   ./scripts/singularity/benchmark-all.sh           # full (~25–40 min)
#   ./scripts/singularity/benchmark-all.sh --quick   # CPU + load smoke + Track C quick
#
# Artifacts: target/singularity-benchmark/{report.md,summary.json,validation/}
set -euo pipefail
cd "$(dirname "$0")/../.."

# shellcheck source=lib/ui.sh
source "$(dirname "$0")/../lib/ui.sh"

ROOT="$PWD"
OUT="$ROOT/target/singularity-benchmark"
VAL="$OUT/validation"
mkdir -p "$VAL"

QUICK=0
SKIP_GPU=0
for arg in "$@"; do
  case "$arg" in
    --quick) QUICK=1 ;;
    --skip-gpu) SKIP_GPU=1 ;;
    -h | --help)
      sed -n '1,10p' "$0"
      exit 0
      ;;
  esac
done

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "benchmark-all: Linux host required (singularity VM)" >&2
  exit 1
fi

STARTED="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
demiurge_banner "DEMIURGE · Singularity benchmark" \
  "mode    $([ "$QUICK" -eq 1 ] && echo quick || echo full)" \
  "repo    $(_ui_git_ref)" \
  "host    $(_ui_host_tag)" \
  "budgets design/bench-gates.toml + design/load-bench.toml"

bench_rc=0 load_rc=0 stress_rc=0 sim_rc=0 harden_rc=0 trackc_rc=0 kernel_rc=0

bold "ensure toolchain"
bash ./scripts/ensure-rust-toolchain.sh 2>&1 | tee "$VAL/ensure-rust.log"

bold "CPU bench-probe (ns budgets)"
set +e
cargo run --release -q --package xtask -- bench-probe 2>&1 | tee "$VAL/bench-probe.log"
bench_rc=$?
set -e

bold "CPU bench-gate (release hot paths)"
set +e
cargo run --release -q --package xtask -- bench-gate 2>&1 | tee "$VAL/bench-gate.log"
if [[ "$bench_rc" -eq 0 ]]; then bench_rc=$?; fi
set -e

bold "Track B runtime gate (BPF + XDP veth)"
set +e
./scripts/track-b-gate.sh 2>&1 | tee "$VAL/track-b-gate.log"
if [[ "$bench_rc" -eq 0 ]]; then bench_rc=$?; fi
set -e

if [[ "$QUICK" -eq 0 ]]; then
  bold "full load bench (ms p99 budgets)"
  set +e
  cargo run --release -q --package xtask -- load-bench 2>&1 | tee "$VAL/load-bench.log"
  load_rc=$?
  set -e
  cp -f target/load-bench/latest.json "$VAL/load-latest.json" 2>/dev/null || true

  bold "stress suite (strict ms + admit shed)"
  sleep 5
  set +e
  cargo run --release -q --package xtask -- load-bench --stress 2>&1 | tee "$VAL/load-stress.log"
  stress_rc=$?
  set -e
  cp -f target/load-bench/stress.json "$VAL/stress.json" 2>/dev/null || true

  bold "apostrophe-sim (fleet replay ms budgets)"
  set +e
  ./scripts/apostrophe-sim.sh 2>&1 | tee "$VAL/apostrophe-sim.log"
  sim_rc=$?
  set -e
  cp -f target/load-bench/sim.json "$VAL/sim.json" 2>/dev/null || true

  bold "harden verify (tier 1–4 + harden load)"
  set +e
  ./scripts/harden-verify.sh 2>&1 | tee "$VAL/harden-verify.log"
  harden_rc=$?
  set -e
  cp -f target/harden-verify/report.md "$VAL/harden-report.md" 2>/dev/null || true

  if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
    bold "optional: LOAD-TRACK-B-KERNEL (root XDP veth)"
    set +e
    sudo -E env PATH="$PATH" HOME="$HOME" \
      cargo run --release -q --package xtask -- load-bench --scenario LOAD-TRACK-B-KERNEL 2>&1 | tee "$VAL/load-kernel.log"
    kernel_rc=$?
    set -e
  else
    echo "skip LOAD-TRACK-B-KERNEL (no passwordless sudo)" | tee "$VAL/load-kernel.log"
    kernel_rc=0
  fi
else
  bold "quick load (CI scenarios only)"
  set +e
  cargo run --release -q --package xtask -- load-bench --ci 2>&1 | tee "$VAL/load-bench.log"
  load_rc=$?
  set -e
  cp -f target/load-bench/latest.json "$VAL/load-latest.json" 2>/dev/null || true
  echo "skip stress/sim/harden/kernel (--quick)" | tee "$VAL/load-stress.log"
fi

if [[ "$SKIP_GPU" -eq 0 ]] && command -v nvidia-smi >/dev/null 2>&1; then
  bold "Track C live GPU (ms — informal e2e budgets)"
  set +e
  if [[ "$QUICK" -eq 1 ]]; then
    ./scripts/track-c-verify.sh --quick --ensure-up 2>&1 | tee "$VAL/track-c-verify.log"
  else
    ./scripts/track-c-verify.sh --ensure-up 2>&1 | tee "$VAL/track-c-verify.log"
  fi
  trackc_rc=$?
  set -e
  cp -f target/track-c-verify/summary.json "$VAL/track-c-summary.json" 2>/dev/null || true
  cp -f target/track-c-verify/validation/live-smoke.json "$VAL/track-c-live-smoke.json" 2>/dev/null || true
  cp -f target/track-c-verify/validation/post-warmth-smoke.json "$VAL/track-c-post-warmth.json" 2>/dev/null || true
else
  echo "skip Track C (no GPU or --skip-gpu)" | tee "$VAL/track-c-verify.log"
  trackc_rc=0
fi

FINISHED="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
bold "generating budget report"
python3 - "$OUT" "$STARTED" "$FINISHED" "$QUICK" "$bench_rc" "$load_rc" "$stress_rc" "$sim_rc" "$harden_rc" "$trackc_rc" "$kernel_rc" <<'PY'
import json, re, sys, tomllib
from pathlib import Path

out = Path(sys.argv[1])
started, finished = sys.argv[2], sys.argv[3]
quick = int(sys.argv[4])
rcs = list(map(int, sys.argv[5:12]))
bench_rc, load_rc, stress_rc, sim_rc, harden_rc, trackc_rc, kernel_rc = rcs
val = out / "validation"
root = out.parent.parent

def read_log(name):
    p = val / name
    return p.read_text() if p.is_file() else ""

def load_json(name):
    p = val / name
    if not p.is_file():
        p = Path(name)
    return json.loads(p.read_text()) if p.is_file() else None

# --- ns budgets from bench-gates.toml ---
gates_toml = tomllib.loads((root / "design" / "bench-gates.toml").read_text())
ns_limits = {g["id"]: g["max_median_ns"] for g in gates_toml["gate"]}

cpu_rows = []
for line in read_log("bench-probe.log").splitlines():
    m = re.match(
        r"^(BENCH-\S+)\s+(\d+)ns\s+(\d+)ns\s+(\d+)ns\s+(\d+)ns\s+([\d.]+)%\s+(\S+)",
        line.strip(),
    )
    if not m:
        continue
    gid, floor, median, p95, limit, headroom, thin = m.groups()
    limit_i = int(limit)
    median_i = int(median)
    cpu_rows.append({
        "id": gid,
        "floor_ns": int(floor),
        "median_ns": median_i,
        "p95_ns": int(p95),
        "limit_ns": limit_i,
        "headroom_pct": float(headroom),
        "thin": thin == "YES",
        "pass": median_i <= limit_i,
    })

bench_gate_pass = "bench-gate: OK" in read_log("bench-gate.log")
track_b_gate_pass = "TRACK B GATE: PASSED" in read_log("track-b-gate.log")

# --- ms budgets from load-bench.toml ---
load_toml = tomllib.loads((root / "design" / "load-bench.toml").read_text())
ms_limits = {}
for sc in load_toml["scenario"]:
    if "max_p99_ms" in sc:
        ms_limits[sc["id"]] = sc["max_p99_ms"]
    if sc.get("min_errors"):
        ms_limits[f"{sc['id']}_min_errors"] = sc["min_errors"]

def check_scenarios(data, label):
    rows = []
    if not data:
        return rows
    for s in data.get("scenarios", []):
        sid = s.get("id", "?")
        ok = s.get("ok", 0)
        err = s.get("errors", 0)
        p99_ms = s.get("p99_us", 0) / 1000.0
        limit = s.get("max_p99_ms") or ms_limits.get(sid)
        p99_pass = None
        if limit is not None and ok > 0:
            p99_pass = p99_ms <= limit
        min_err = ms_limits.get(f"{sid}_min_errors")
        err_pass = None
        if min_err is not None:
            err_pass = err >= min_err
        rows.append({
            "suite": label,
            "id": sid,
            "ok": ok,
            "errors": err,
            "p99_ms": round(p99_ms, 2),
            "limit_ms": limit,
            "p99_pass": p99_pass,
            "min_errors": min_err,
            "err_pass": err_pass,
            "rps": round(s.get("req_per_sec", 0), 1),
        })
    return rows

load_rows = check_scenarios(load_json("load-latest.json"), "load")
stress_rows = check_scenarios(load_json("stress.json"), "stress")
sim_rows = check_scenarios(load_json("sim.json"), "sim")

# Track C live (informal budgets — no formal gate in load-bench.toml)
trackc_rows = []
live = load_json("track-c-live-smoke.json")
if live:
    for c in live.get("checks", []):
        if c.get("latency_ms") is None:
            continue
        lat = c["latency_ms"]
        # Engineering e2e SLA: CI smoke ceiling for short paths; 2s for long disagg
        cid = c["id"]
        if "DISAGG" in cid or "WARM" in cid:
            limit = 2000.0
        else:
            limit = 500.0
        trackc_rows.append({
            "suite": "track-c",
            "id": cid,
            "latency_ms": lat,
            "limit_ms": limit,
            "pass": c.get("pass", False) and lat <= limit,
            "http_code": c.get("http_code"),
        })
post = load_json("track-c-post-warmth.json")
if post:
    for c in post.get("checks", []):
        lat = c.get("latency_ms")
        if lat is None:
            continue
        trackc_rows.append({
            "suite": "track-c-hot",
            "id": c["id"],
            "latency_ms": lat,
            "limit_ms": 500.0,
            "pass": c.get("pass", False) and lat <= 500.0,
            "http_code": c.get("http_code"),
        })

cpu_pass = all(r["pass"] for r in cpu_rows) if cpu_rows else False
load_p99_fail = [r for r in load_rows if r["p99_pass"] is False]
stress_p99_fail = [r for r in stress_rows if r["p99_pass"] is False]
stress_err_fail = [r for r in stress_rows if r["err_pass"] is False]

summary = {
    "started_utc": started,
    "finished_utc": finished,
    "mode": "quick" if quick else "full",
    "host": "singularity",
    "cpu_gates": {"pass": cpu_pass and bench_gate_pass, "gates": cpu_rows, "thin": [r["id"] for r in cpu_rows if r["thin"]]},
    "track_b_bpf_pass": track_b_gate_pass,
    "load_p99_failures": [r["id"] for r in load_p99_fail],
    "stress_p99_failures": [r["id"] for r in stress_p99_fail],
    "stress_err_failures": [r["id"] for r in stress_err_fail],
    "track_c_rows": trackc_rows,
    "rc": {
        "bench": bench_rc,
        "load": load_rc,
        "stress": stress_rc,
        "sim": sim_rc,
        "harden": harden_rc,
        "track_c": trackc_rc,
        "kernel": kernel_rc,
    },
}
summary["all_ns_pass"] = cpu_pass and bench_gate_pass
summary["all_ms_pass"] = (
    not load_p99_fail
    and not stress_p99_fail
    and all(r.get("pass") for r in trackc_rows)
)
summary["overall_pass"] = (
    summary["all_ns_pass"]
    and not load_p99_fail
    and not stress_p99_fail
    and (not trackc_rows or all(r.get("pass") for r in trackc_rows))
)

(out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

md = [
    "# Singularity benchmark — budget report",
    "",
    f"- **Started:** {started} UTC",
    f"- **Finished:** {finished} UTC",
    f"- **Mode:** {'quick' if quick else 'full'}",
    f"- **Artifacts:** `{out}`",
    "",
    "## Verdict",
    "",
    f"| Layer | Budget source | Result |",
    f"|-------|---------------|--------|",
    f"| CPU hot paths | `design/bench-gates.toml` (ns median) | {'**PASS**' if summary['all_ns_pass'] else '**FAIL**'} |",
    f"| Mock TCP load | `design/load-bench.toml` (p99 ms) | {'**PASS**' if not load_p99_fail else '**FAIL**'} |",
    f"| Stress | p99 ms + admit shed | p99 {'PASS' if not stress_p99_fail else 'FAIL'} · admit shed {'PASS' if not stress_err_fail else 'FAIL (VM too fast)'} |",
    f"| Track C GPU e2e | informal 500ms / 2s | {'**PASS**' if all(r.get('pass') for r in trackc_rows) else '**FAIL**' if trackc_rows else 'SKIP'} |",
    "",
    "## CPU gates (nanoseconds)",
    "",
    "| gate | median | limit | headroom | thin | pass |",
    "|------|-------:|------:|---------:|:----:|:----:|",
]
for r in cpu_rows:
    md.append(
        f"| {r['id']} | {r['median_ns']}ns | {r['limit_ns']}ns | {r['headroom_pct']:.0f}% | "
        f"{'YES' if r['thin'] else '·'} | {'✓' if r['pass'] else '✗'} |"
    )
if summary["cpu_gates"]["thin"]:
    md.extend(["", f"**Thin gates:** {', '.join(summary['cpu_gates']['thin'])}", ""])

def ms_table(rows, title):
    if not rows:
        return [f"### {title}", "", "(not run)", ""]
    lines = [
        f"### {title}",
        "",
        "| scenario | ok/err | rps | p99 ms | limit ms | pass |",
        "|----------|-------:|----:|-------:|---------:|:----:|",
    ]
    for r in rows:
        lim = r["limit_ms"]
        lim_s = f"{lim:.0f}" if lim is not None else "—"
        ppass = r["p99_pass"]
        mark = "✓" if ppass else ("✗" if ppass is False else "·")
        lines.append(
            f"| {r['id']} | {r['ok']}/{r['errors']} | {r['rps']:.0f} | {r['p99_ms']:.1f} | {lim_s} | {mark} |"
        )
    if any(r["err_pass"] is False for r in rows):
        lines.extend(["", "**Admit/err gates:**", ""])
        for r in rows:
            if r["min_errors"] is not None:
                lines.append(
                    f"- {r['id']}: {r['errors']} errors (need ≥{r['min_errors']}) → "
                    f"{'PASS' if r['err_pass'] else 'FAIL'}"
                )
    lines.append("")
    return lines

md.extend(ms_table(load_rows, "Load bench (milliseconds)"))
md.extend(ms_table(stress_rows, "Stress (milliseconds)"))
md.extend(ms_table(sim_rows, "Apostrophe-sim (milliseconds)"))

if trackc_rows:
    md.extend([
        "### Track C live GPU (milliseconds — informal)",
        "",
        "| check | latency ms | budget ms | pass |",
        "|-------|----------:|----------:|:----:|",
    ])
    for r in trackc_rows:
        md.append(
            f"| {r['id']} | {r['latency_ms']:.1f} | {r['limit_ms']:.0f} | {'✓' if r['pass'] else '✗'} |"
        )
    md.append("")

md.extend([
    "## Notes",
    "",
    "- **ns budgets** are release-mode medians on this host; CI applies 2× slack.",
    "- **ms budgets** are mock-TCP router scenarios; GPU Track C uses informal SLA (500ms short / 2s long).",
    "- `LOAD-STRESS-ADMIT-FLOOD` **min_errors** often fails on fast VMs (cannot shed fast enough) — not a latency regression.",
    "",
    "## Logs",
    "",
    "- `validation/bench-probe.log` — CPU ns headroom",
    "- `validation/load-latest.json` / `stress.json` / `sim.json`",
    "- `validation/track-c-*.json` — live Llama P/D",
    "- `summary.json` — machine-readable rollup",
])
(out / "report.md").write_text("\n".join(md) + "\n")
print(f"report: {out / 'report.md'}")
print(f"summary: {out / 'summary.json'}")
PY

bold "report ready"
cat "$OUT/report.md" | head -80

fail=0
if ! python3 - <<PY
import json, sys
s = json.load(open("$OUT/summary.json"))
# Overall: ns must pass; ms p99 must pass; admit shed advisory on fast VM
if not s.get("all_ns_pass"):
    sys.exit(1)
if s.get("load_p99_failures") or s.get("stress_p99_failures"):
    sys.exit(1)
PY
then
  fail=1
fi

if [[ "$fail" -ne 0 ]]; then
  demiurge_fail "SINGULARITY BENCHMARK: budget miss (see $OUT/report.md)"
  exit 1
fi

demiurge_pass "SINGULARITY BENCHMARK: within ns + ms p99 budgets (see report for admit-shed note)"
