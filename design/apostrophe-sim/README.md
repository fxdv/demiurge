# 'sim — Fleet Simulation Spinoff

**'sim** (Apostrophe Sim) is a Demiurge spinoff for production-shaped fleet testing
without a GPU rack. It closes the loop between fleet traces and live TCP load against
the real router stack.

## Platform

| Tier | macOS | Linux (native / VM) | Notes |
|------|:-----:|:-------------------:|-------|
| **L1** `SIM-FLEET-REPLAY` | ✅ | ✅ | Mock TCP — same as load-bench Track A |
| **L2** `SIM-FLEET-HETERO` | ✅ | ✅ | Jitter + tier skew + simulated netem |
| **L3** Docker compose | — | ✅ | Privileged Linux container only |

No GPU, root, or XDP required for L1/L2. Your **load/stress runs on the Linux VM**;
**'sim L1/L2 runs on both macOS and Linux** (I ran it locally on macOS; your VM results
are valid too). Use L3 Docker when you want the same environment as `linux-nightly`.

```text
  trace JSONL  ──►  window knobs  ──►  live load-bench  ──►  π / p99 gates
       │                │                    │
       └─ fleet-pilot shadow π* ────────────┴─ held-out correlation gate
```

## Tiers

| Tier | ID | What it simulates |
|------|-----|-------------------|
| **L1** | `SIM-FLEET-REPLAY` | Trace windows drive concurrency, prefill mix, token profile |
| **L2** | `SIM-FLEET-HETERO` | Tier-skewed backend delays, jitter, remote netem |
| **L3** | Docker | Linux privileged container (`scripts/apostrophe-sim/docker-compose.yml`) |

## Run

```bash
./scripts/apostrophe-sim.sh
# or
cargo run --release -q --package xtask -- 'sim
```

Reports: `target/load-bench/sim.json` + `sim.pseudo`

## Trace format

JSONL rows (same as fleet-pilot):

```json
{"ts_ms":0,"q_prefill":0.90,"q_decode":0.20,"kv_decode":0.22,"prefill_heavy":true,"held_out":false}
```

Fields map to load knobs via `demiurge-control::fleet_sim::window_knobs`.

## Gates

- **p99** — per-scenario soft ceiling
- **dataplane π** — actuation tracks prefill-heavy windows (`min_dataplane_pi`)
- **fleet replay** — shadow π* correlates on held-out windows; live π separates heavy/light

## What 'sim does not claim

Mock TCP backends — not real GPU prefill/decode. Proof ≠ production economics.
Track C (reference GPU rack) remains the production exit gate.

## Files

| Path | Role |
|------|------|
| `design/apostrophe-sim/README.md` | This spec |
| `design/traces/synthetic-fleet.jsonl` | Bundled synthetic trace |
| `crates/demiurge-control/src/fleet_sim.rs` | Trace → knobs, gates |
| `xtask/src/apostrophe_sim.rs` | CLI entrypoint |
| `scripts/apostrophe-sim.sh` | Local runner |
| `scripts/apostrophe-sim/docker-compose.yml` | L3 Linux container |
