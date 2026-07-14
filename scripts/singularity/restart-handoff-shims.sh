#!/usr/bin/env bash
# Restart prefill handoff shims only (keeps vLLM workers running).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LOG_DIR="${VLLM_LOG_DIR:-$HOME/vllm-workers}"
BYTES_PER_TOKEN="${DEMIURGE_BYTES_PER_TOKEN:-128}"

mkdir -p "$LOG_DIR"

pkill -f 'prefill_handoff_shim.py --listen 9001' >/dev/null 2>&1 || true
pkill -f 'prefill_handoff_shim.py --listen 9002' >/dev/null 2>&1 || true
sleep 1

start_shim() {
  local listen=$1 backend=$2
  local log="$LOG_DIR/shim-${listen}.log"
  nohup python3 "$ROOT/scripts/singularity/prefill_handoff_shim.py" \
    --listen "$listen" \
    --backend "$backend" \
    --bytes-per-token "$BYTES_PER_TOKEN" \
    >"$log" 2>&1 &
  echo "shim listen=$listen backend=$backend pid=$! log=$log"
}

start_shim 9001 9101
start_shim 9002 9102
sleep 1

code=$(curl -s -o /dev/null -w "%{http_code}" \
  -X POST "http://127.0.0.1:9001/v1/chat/completions" \
  -H "Content-Type: application/json" \
  -H "X-Demiurge-Tokens: 64" \
  -d '{"model":"x","messages":[{"role":"user","content":"hi"}],"max_tokens":1}' 2>/dev/null || echo 000)
if [[ "$code" != "200" ]]; then
  echo "WARN: shim handoff probe returned HTTP $code (vLLM on 9101 may still be warming)" >&2
fi
