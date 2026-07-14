# Track D — Fleet economics & market evidence

**Purpose.** Turn engineering proof (Tracks A–C) into **market-credible numbers**:
$/token, goodput, and OOM-prevention delta at fleet scale on real GPUs.

**Not in CI.** Track D runs are partner/reference-host exercises. Results are frozen under
[`design/validation/`](../validation/) when gates pass.

**Gate definitions:** [`design/fleet-economics.toml`](../fleet-economics.toml)

---

## Exit criteria (Track D complete)

Track D is **closed** when all three primary scenarios are **PASS** on the **same**
reference fleet (documented in one validation archive):

| Scenario ID | Market claim enabled |
|-------------|----------------------|
| `FLEET-AB-GOODPUT` | “More tokens out of the same GPUs” |
| `FLEET-AB-OOM-BURST` | “Fewer silent OOMs under burst prefill” |
| `FLEET-AB-COST` | “Lower $ per million output tokens” |

Supporting: `FLEET-AB-WARMTH` (prefix locality evidence for the goodput story).

---

## A/B protocol

### 1. Fleet freeze

Both arms must use the **identical** hardware and model stack:

| Item | Rule |
|------|------|
| GPUs | Same count, SKU, driver, power cap |
| Model | Same weights, quantization, max model len |
| vLLM | Same version, tensor parallel, batch limits |
| Traffic | Same client generator, same request mix |
| Duration | ≥ `settings.min_run_minutes` per arm per scenario |
| Repeats | ≥ `settings.min_repeats` full A/B pairs |

**Only the router policy changes** between baseline and treatment.

### 2. Arms

| Arm | `mode` | Implementation |
|-----|--------|----------------|
| **Baseline A** | `round_robin` | Fixed RR across decode (and prefill if disagg); or nginx upstream RR to vLLM :9003–9004 only |
| **Baseline B** | `least_conn` | Pick minimum `inflight` backend; no warmth, no ledger |
| **Treatment** | `demiurge` | Production Demiurge: classify, ledger, warmth, pairing, admit |

Record which baseline is the **primary comparator** in the archive README (usually `round_robin` for greenfield, `least_conn` for ops teams already doing smart LB).

### 3. Workloads

#### `shared_prefix_agent` (goodput + warmth)

Mimics agent traffic: long shared system prompt, short per-turn suffix.

```bash
export BENCH_PROMPT_TOKENS=2048
export BENCH_WARMUP=32
export BENCH_RUNS=200
export BENCH_CONC=32
python3 scripts/singularity/warmth-prefix-bench.py   # treatment arm
```

Vary only `DEMIURGE_ROUTER` target (treatment vs baseline proxy). Client must send
`X-Demiurge-Tokens` consistent with actual prompt size.

#### `burst_prefill` (OOM)

Ramp concurrency over `ramp_seconds` with long prompts (`prompt_tokens=4096`, `max_tokens=8`).

- **Treatment:** KV ledger + admit shed → expect **503s**, zero CUDA OOM in vLLM logs.
- **Baseline:** expect ≥1 OOM or worker restart (document in archive).

Scrape signals:

```bash
# vLLM / CUDA OOM indicators
journalctl -u demiurge-vllm-pd --since "1 hour ago" | rg -i "oom|out of memory|CUDA error"
grep -i "out of memory" ~/vllm-workers/vllm-*.log
```

#### `mixed_phase` (cost)

60% short (≤512 tok → colocated path), 40% long (disagg). Measure **output tokens**
and wall time per arm; apply operator **$/GPU-hour** tariff.

---

## Metrics (definitions)

| Metric | Formula | Source |
|--------|---------|--------|
| **goodput_tokens_per_gpu_hour** | `sum(output_tokens) / (gpu_count × wall_hours)` | Router access log + response `usage.completion_tokens` |
| **oom_or_cuda_oom_events** | Count of OOM lines in vLLM logs + worker restarts | Log scrape (see above) |
| **usd_per_million_output_tokens** | `(gpu_count × $/gpu_hour × wall_hours) / (output_tokens / 1e6)` | Operator tariff + goodput |
| **warmth_hit_rate** | Fraction of post-warmup requests hitting dominant prefill worker | `warmth-prefix-bench.py` skew % |
| **error_rate** | `(non_2xx) / total` | Client harness |
| **p99_latency_ms** | p99 end-to-end from client | Harness per-request timings |

**Goodput** counts only **successful** completions (HTTP 200 with ≥1 output token).
503 sheds under overload are **not** failures for treatment OOM scenario — they are
the desired graceful path.

---

## Runbook (singularity reference)

Prerequisites: fleet up (`demiurge-vllm-pd`, `demiurge-router`), model warm.

```bash
# 1. Treatment arm — goodput
export DEMIURGE_ROUTER=http://127.0.0.1:8080
./scripts/singularity/warmth-prefix-bench.py | tee /tmp/track-d-treatment-goodput.json

# 2. Baseline arm — point clients at RR proxy (document setup in archive)
export DEMIURGE_ROUTER=http://127.0.0.1:9090   # example nginx RR → :9003/:9004
# ... same bench command ...

# 3. Burst OOM — treatment then baseline (separate windows; reboot fleet between)
export BENCH_CONC=64 BENCH_PROMPT_TOKENS=4096
# custom burst client or load generator (document in archive)

# 4. Aggregate → summary.json (schema below)
```

Future: `./scripts/track-d-verify.sh` will automate arms, repeats, and gate checks.

---

## Archive schema (`summary.json`)

```json
{
  "track": "D",
  "archive_date": "2026-MM-DD",
  "host": "singularity",
  "fleet": { "gpus": "4x Tesla V100-SXM3-32GB", "model": "Meta-Llama-3.1-8B-Instruct" },
  "tariff_usd_per_gpu_hour": 2.50,
  "scenarios": [
    {
      "id": "FLEET-AB-GOODPUT",
      "baseline": { "mode": "round_robin", "goodput_tokens_per_gpu_hour": 120000 },
      "treatment": { "mode": "demiurge", "goodput_tokens_per_gpu_hour": 138000 },
      "delta_pct": 15.0,
      "p99_regression_ratio": 1.02,
      "gate": "PASS"
    }
  ],
  "track_d_exit": "PASS"
}
```

Store under `design/validation/<host>-track-d-<date>/` with `README.md` narrative.

---

## Statistical rigor

1. **Three repeats** minimum per scenario per arm; report median and IQR.
2. **Cooldown:** 5 min between repeats; re-warm model between baseline↔treatment swaps.
3. **Fairness cap:** treatment p99 ≤ `max_p99_regression_ratio` × baseline p99.
4. **Honest labeling:** archive header must state if shims, TCP handoff, or localhost-only.

---

## What Track D does *not* claim

- Replacing vLLM/TGI (Demiurge is in front of workers).
- Multi-tenant SaaS readiness (tenant auth on wire is Track C).
- NIC XDP production (Track B exit).

Track D **does** claim: **measurable fleet economics** vs phase-blind baselines on the
same hardware — the minimum bar for market disruption evidence.
