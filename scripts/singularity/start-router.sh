#!/usr/bin/env bash
# Start demiurge-router with KV ledger + live state plane for singularity P/D.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

if [[ ! -x target/release/demiurge-router ]]; then
  cargo build --release -q -p demiurge-router
fi

pkill -f target/release/demiurge-router >/dev/null 2>&1 || true
sleep 1

export DEMIURGE_LISTEN="${DEMIURGE_LISTEN:-127.0.0.1:8080}"
export DEMIURGE_ADMIT_MODE="${DEMIURGE_ADMIT_MODE:-userspace}"
export DEMIURGE_PREFILL="${DEMIURGE_PREFILL:-pf0@127.0.0.1:9001@0.01,pf1@127.0.0.1:9002@0.01}"
export DEMIURGE_DECODE="${DEMIURGE_DECODE:-dc0@127.0.0.1:9003@0.01,dc1@127.0.0.1:9004@0.01}"
export DEMIURGE_BYTES_PER_TOKEN="${DEMIURGE_BYTES_PER_TOKEN:-128}"
# ~30 GiB fleet decode KV budget (2× V100 decode workers)
export DEMIURGE_DECODE_KV_CAPACITY_BYTES="${DEMIURGE_DECODE_KV_CAPACITY_BYTES:-32212254720}"
export DEMIURGE_STATE_PLANE="${DEMIURGE_STATE_PLANE:-1}"
export DEMIURGE_BANNER="${DEMIURGE_BANNER:-0}"

LOG="${DEMIURGE_ROUTER_LOG:-$HOME/router.log}"
nohup target/release/demiurge-router >"$LOG" 2>&1 &
echo "demiurge-router pid=$! log=$LOG"
sleep 1
ss -ltnp 2>/dev/null | grep :8080 || netstat -ltnp 2>/dev/null | grep :8080 || true
