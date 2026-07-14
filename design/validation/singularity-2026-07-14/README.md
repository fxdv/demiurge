# Singularity validation archive — 2026-07-14

**Host:** singularity @ 176.123.167.143  
**GPUs:** 4× Tesla V100-SXM3-32GB  
**Model:** Meta-Llama-3.1-8B-Instruct (NousResearch mirror)  
**Topology:** 2 prefill (9001/9002) + 2 decode (9003/9004) via demiurge-router :8080

## Track C P/D proof gate

Single PASS/FAIL entry point (logic + live fleet):

```bash
cd ~/demiurge && git pull
./scripts/track-c-verify.sh --ensure-up
```

Artifacts: `target/track-c-verify/report.md`, `summary.json`

| Stage | ID | What it proves |
|-------|-----|----------------|
| Logic | TC-MIG-UNIT | Phase 6 migration cutover budget (unit) |
| Logic | TC-P7-UNIT | Phase 7 cache-domain isolation on router |
| Logic | TC-P8-UNIT | Phase 8 corrector graduation FSM |
| Logic | TC-KV-UNIT / TC-WARM-UNIT | KV ledger + warmth routing |
| Live | TC-LIVE-SMOKE | models + colocated + disagg via router |
| Live | TC-WARMTH-SKEW | Prefix warmth skew on prefill workers |

**Passing this gate** closes the **Track C P/D proof slice** on reference GPU hardware.  
**Full Track C roadmap closure** still requires RDMA prod handoff, fleet-measured migration p99, live corrector wiring, and tenant auth on production traffic (listed in the gate report).

### Verified run — 2026-07-14

| Item | Value |
|------|-------|
| Command | `./scripts/track-c-verify.sh` |
| Started (UTC) | 2026-07-14T09:20:22Z |
| Duration | ~28s |
| Result | **PASS** (all logic + live stages) |
| Branch | `singularity-real-pd` |

| Live check | Result | Latency |
|------------|--------|---------|
| TC-LIVE-COLOCATED (`X-Demiurge-Tokens: 64`) | PASS | p50 ~120ms |
| TC-LIVE-DISAGG (`X-Demiurge-Tokens: 1024`) | PASS | p50 ~125ms |
| TC-WARMTH-SKEW (32 disagg @ 2048 tok) | PASS | p50 ~1.19s; 100% on `9101` |

Report on host: `~/demiurge/target/track-c-verify/report.md`

## Track B benches (mock TCP — engineering proof)

| Stage | Result |
|-------|--------|
| gate + Track B gate | PASS |
| load-bench (12 scenarios) | PASS |
| stress (4 scenarios) | FAIL — `LOAD-STRESS-ADMIT-FLOOD` shed count (9 vs min 50) |
| apostrophe-sim | PASS |
| bench-flame | PASS (thin CPU gates on shared VM) |

Artifacts on host: `~/track-b-verify.log`, `~/demiurge/target/track-b-verify/`

## Live Llama P/D (verified via `track-c-verify`)

| Path | Status | Notes |
|------|--------|-------|
| Colocated (`X-Demiurge-Tokens: 64`) | PASS | decode pool via router :8080 |
| Disaggregated (`X-Demiurge-Tokens: 1024`) | PASS | 2-hop P/D (shim → decode) |
| Warmth skew (TC-WARMTH-SKEW) | PASS | 100% prefill on worker `9101` |

## Path A rollout components

- **C2:** `DEMIURGE_DECODE_KV_CAPACITY_BYTES` + `DEMIURGE_STATE_PLANE` in `demiurge-router`
- **C1:** `scripts/singularity/prefill_handoff_shim.py` on prefill ports
- **C3:** Live warmth recording via `StatePlane`
- **C4:** `scripts/singularity/warmth-prefix-bench.py`
- **O1/O2:** systemd units + `scripts/singularity/bootstrap.sh`

Manual restart:

```bash
~/demiurge/scripts/singularity/start-vllm-pd.sh
~/demiurge/scripts/singularity/start-router.sh
./scripts/track-c-verify.sh
```
