# Track D scripts

Fleet economics A/B runs are **manual** until `track-d-verify.sh` ships.

**Protocol:** [`design/track-d/README.md`](../../design/track-d/README.md)  
**Gates:** [`design/fleet-economics.toml`](../../design/fleet-economics.toml)

**Reuse today:**

- `scripts/singularity/warmth-prefix-bench.py` — shared-prefix goodput / warmth arm
- `scripts/singularity/benchmark-all.sh` — ns/ms engineering rollup (not Track D exit)
- `scripts/track-c-verify.sh` — P/D proof prerequisite

**Planned:** `./scripts/track-d-verify.sh` → `target/track-d-verify/report.md` + optional archive under `design/validation/`.
