# Singularity GPU fleet — Llama 3.1 8B P/D

Reference host for **Track C** proof: real vLLM backends, Demiurge KV ledger, live warmth.

## Quick start (on VM)

```bash
git -C ~/demiurge pull
./scripts/singularity/bootstrap.sh      # first time only
./scripts/singularity/start-vllm-pd.sh  # 4× vLLM + 2× prefill shims
./scripts/singularity/start-router.sh   # KV ledger + state plane
```

## Topology

| GPU | Role | Router port | Process |
|-----|------|-------------|---------|
| 0 | prefill | 9001 | handoff shim → vLLM :9101 |
| 1 | prefill | 9002 | handoff shim → vLLM :9102 |
| 2 | decode | 9003 | vLLM direct |
| 3 | decode | 9004 | vLLM direct |

Router: `127.0.0.1:8080` with `DEMIURGE_DECODE_KV_CAPACITY_BYTES=30GiB`.

## Benches

```bash
./scripts/singularity/benchmark-all.sh       # full ns + ms budget rollup (~10 min)
./scripts/track-c-verify.sh              # full Track C P/D proof gate
./scripts/track-c-verify.sh --quick        # logic + live smoke only
./scripts/track-c-verify.sh --ensure-up    # start vLLM/router then verify
python3 scripts/singularity/warmth-prefix-bench.py
python3 scripts/singularity/track-c-live-smoke.py
```

Report: `target/track-c-verify/report.md`, `target/singularity-benchmark/report.md`

## sudo (passwordless)

`user1` has passwordless sudo via cloud-init (`/etc/sudoers.d/90-cloud-init-users`).
Use it for systemd and **root-only** Track B kernel XDP load:

```bash
sudo -E env PATH="$PATH" HOME="$HOME" \
  cargo run --release -q --package xtask -- load-bench --scenario LOAD-TRACK-B-KERNEL
```

`benchmark-all.sh` runs this automatically when `sudo -n true` succeeds.

## systemd

```bash
sudo cp scripts/singularity/systemd/*.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now demiurge-vllm-pd demiurge-router
```

Validation archive: [`design/validation/singularity-2026-07-14/`](../../design/validation/singularity-2026-07-14/README.md) — **Track C P/D proof gate PASS** (2026-07-14)
