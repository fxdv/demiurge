# Singularity validation archive — 2026-07-14

**Host:** singularity @ 176.123.167.143  
**GPUs:** 4× Tesla V100-SXM3-32GB  
**Model:** Meta-Llama-3.1-8B-Instruct (NousResearch mirror)  
**Topology:** 2 prefill (9001/9002) + 2 decode (9003/9004) via demiurge-router :8080

## Track B benches (mock TCP — engineering proof)

| Stage | Result |
|-------|--------|
| gate + Track B gate | PASS |
| load-bench (12 scenarios) | PASS |
| stress (4 scenarios) | FAIL — `LOAD-STRESS-ADMIT-FLOOD` shed count (9 vs min 50) |
| apostrophe-sim | PASS |
| bench-flame | PASS (thin CPU gates on shared VM) |

Artifacts on host: `~/track-b-verify.log`, `~/demiurge/target/track-b-verify/`

## Live Llama P/D (pre-handoff-shim baseline)

| Path | p50 | p99 |
|------|-----|-----|
| Colocated (`X-Demiurge-Tokens: 64`) | 87ms | 230ms |
| Disaggregated (`X-Demiurge-Tokens: 1024`) | 822ms | 971ms |

Log: `~/vllm-workers/llama-pd-bench.log`

## Path A rollout (this commit)

- **C2:** `DEMIURGE_DECODE_KV_CAPACITY_BYTES` + `DEMIURGE_STATE_PLANE` in `demiurge-router`
- **C1:** `scripts/singularity/prefill_handoff_shim.py` on prefill ports
- **C3:** Live warmth recording via `StatePlane`
- **C4:** `scripts/singularity/warmth-prefix-bench.py`
- **O1/O2:** systemd units + `scripts/singularity/bootstrap.sh`

Re-run after deploy:

```bash
~/demiurge/scripts/singularity/start-vllm-pd.sh
~/demiurge/scripts/singularity/start-router.sh
python3 ~/demiurge/scripts/singularity/warmth-prefix-bench.py
```
