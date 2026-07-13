#!/usr/bin/env bash
# Start 4× Llama vLLM workers + prefill handoff shims on singularity P/D topology.
#
#   GPU 0-1: vLLM @ 9101-9102, shim @ 9001-9002 (prefill → router)
#   GPU 2-3: vLLM @ 9003-9004 (decode, direct)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
VENV="${VLLM_VENV:-$HOME/1cat-venv}"
MODEL="${VLLM_MODEL:-NousResearch/Meta-Llama-3.1-8B-Instruct}"
SERVED="${VLLM_SERVED_NAME:-Meta-Llama-3.1-8B-Instruct}"
MAX_LEN="${VLLM_MAX_MODEL_LEN:-8192}"
LOG_DIR="${VLLM_LOG_DIR:-$HOME/vllm-workers}"
BYTES_PER_TOKEN="${DEMIURGE_BYTES_PER_TOKEN:-128}"

mkdir -p "$LOG_DIR"

if [[ ! -x "$VENV/bin/python" ]]; then
  echo "missing vLLM venv at $VENV — run scripts/singularity/bootstrap.sh first" >&2
  exit 1
fi

# shellcheck source=/dev/null
source "$VENV/bin/activate"

pkill -f prefill_handoff_shim.py >/dev/null 2>&1 || true
pkill -f vllm.entrypoints.openai.api_server >/dev/null 2>&1 || true
sleep 2

start_vllm() {
  local gpu=$1 port=$2
  local log="$LOG_DIR/vllm-${port}.log"
  CUDA_DEVICE_ORDER=PCI_BUS_ID CUDA_VISIBLE_DEVICES=$gpu \
  VLLM_ATTENTION_BACKEND=FLASH_ATTN_V100 \
  nohup python -m vllm.entrypoints.openai.api_server \
    --model "$MODEL" \
    --served-model-name "$SERVED" \
    --dtype float16 \
    --max-model-len "$MAX_LEN" \
    --host 127.0.0.1 \
    --port "$port" \
    >"$log" 2>&1 &
  echo "vllm gpu=$gpu port=$port pid=$! log=$log"
}

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

# Prefill: internal vLLM + external shim
start_vllm 0 9101
start_vllm 1 9102
start_shim 9001 9101
start_shim 9002 9102

# Decode: router-facing ports are vLLM directly
start_vllm 2 9003
start_vllm 3 9004

echo "waiting for health (up to 3 min)..."
for _ in $(seq 1 36); do
  ok=0
  for p in 9001 9002 9003 9004; do
    code=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:${p}/health" 2>/dev/null || echo 000)
    if [[ "$code" == "200" ]]; then ok=$((ok + 1)); fi
  done
  if [[ "$ok" -eq 4 ]]; then
    echo "all workers healthy"
    exit 0
  fi
  sleep 5
done

echo "WARN: not all workers healthy yet — check $LOG_DIR/*.log" >&2
exit 1
