# Singularity validation archive — 2026-07-14

**Host:** singularity @ 176.123.167.143  
**GPUs:** 4× Tesla V100-SXM3-32GB  
**Model:** Meta-Llama-3.1-8B-Instruct (NousResearch mirror)  
**Topology:** 2 prefill (9001/9002) + 2 decode (9003/9004) via demiurge-router :8080

## Track C P/D proof gate — PASS

```bash
cd ~/demiurge && git pull
./scripts/track-c-verify.sh --ensure-up
```

Artifacts on host: `target/track-c-verify/report.md`, `summary.json`

| Stage | ID | Result |
|-------|-----|--------|
| Logic | TC-MIG / TC-P7 / TC-P8 / TC-KV / TC-WARM / TC-RDMA-SHADOW | PASS |
| Live | TC-LIVE-SMOKE | PASS |
| Live | TC-WARMTH-SKEW | PASS |
| Live | TC-HOT-SHORT-32/64 (post-warmth disagg) | PASS |

### Verified runs

| Run (UTC) | Command | Colocated | Disagg | Warmth / hot-short |
|-----------|---------|-----------|--------|-------------------|
| 09:20 | `track-c-verify.sh` | ~120ms | ~125ms | 100% on `9101` |
| 10:25–10:26 | `--ensure-up` + post-warmth | 261ms | 264ms | 32tok 189ms, 64tok 134ms |

**Passing this gate** closes the **Track C P/D proof slice**. Full Track C closure still requires RDMA prod, migration p99, live corrector, tenant auth, fleet actuation.

## Full benchmark rollup (`benchmark-all.sh`) — PASS (ns + ms p99)

**Run:** 2026-07-14T11:55–12:04 UTC · ~9 min · `target/singularity-benchmark/`

| Layer | Budget | Result |
|-------|--------|--------|
| CPU (11 gates) | `design/bench-gates.toml` ns | **PASS** (6 thin gates on shared VM) |
| Load (12 scenarios) | `design/load-bench.toml` p99 ms | **PASS** (tightest: MIXED-PHASE 21.5ms / 150ms) |
| Stress p99 | up to 5000ms | **PASS** |
| Stress admit shed | min 50 errors on ADMIT-FLOOD | **FAIL** (0 sheds — VM too fast) |
| Apostrophe-sim | 3 scenarios | **PASS** |
| Track C live GPU | informal 500ms / 2s | **PASS** |

Machine-readable rollup: [`summary.json`](summary.json)

## Kernel XDP (veth) — PASS

```bash
sudo -E env PATH="$PATH" HOME="$HOME" \
  cargo run --release -q --package xtask -- load-bench --scenario LOAD-TRACK-B-KERNEL
```

| Metric | Value |
|--------|------:|
| p99 | 3.23ms |
| Limit | 300ms |
| Errors | 0 |

**Not in scope:** production NIC (`ens*`) XDP under saturation — Track B exit gate remains open.

## Track B (mock TCP)

| Stage | Result |
|-------|--------|
| gate + Track B gate | PASS |
| load-bench (12 scenarios) | PASS |
| stress p99 | PASS |
| stress ADMIT-FLOOD shed count | FAIL (fast VM) |

## systemd fleet

```bash
sudo cp scripts/singularity/systemd/*.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now demiurge-vllm-pd demiurge-router
```

Units: `demiurge-vllm-pd` (oneshot, waits for vLLM health) → `demiurge-router` (simple, restart on failure).

`user1` has passwordless sudo via `/etc/sudoers.d/90-cloud-init-users`.

## Ops scripts

| Script | Purpose |
|--------|---------|
| `scripts/singularity/benchmark-all.sh` | Full ns + ms budget report |
| `scripts/singularity/restart-handoff-shims.sh` | Shim-only restart (no vLLM pkill) |
| `scripts/track-c-verify.sh` | P/D proof gate |

## Path A/C components shipped

- **C1:** `prefill_handoff_shim.py` on 9001/9002
- **C2:** KV ledger + state plane env on router
- **C3:** Live warmth recording
- **C4:** `warmth-prefix-bench.py`
- **O1/O2:** systemd + `bootstrap.sh`
