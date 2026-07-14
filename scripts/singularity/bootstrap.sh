#!/usr/bin/env bash
# One-time bootstrap for singularity GPU host (Ubuntu 24.04, 4× V100).
# Idempotent where possible. Run as user1 on the VM.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
VENV="${VLLM_VENV:-$HOME/1cat-venv}"
MODEL="${VLLM_MODEL:-NousResearch/Meta-Llama-3.1-8B-Instruct}"

echo "==> Demiurge singularity bootstrap"
echo "    repo=$ROOT venv=$VENV model=$MODEL"

if ! command -v cargo >/dev/null 2>&1; then
  echo "==> installing Rust"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
fi

if [[ ! -d "$ROOT/.git" ]]; then
  echo "clone demiurge to $ROOT first" >&2
  exit 1
fi

echo "==> build demiurge-router"
cd "$ROOT"
cargo build --release -q -p demiurge-router

if [[ ! -x "$VENV/bin/python" ]]; then
  echo "==> create 1Cat vLLM venv"
  python3 -m venv "$VENV"
  # shellcheck source=/dev/null
  source "$VENV/bin/activate"
  pip install -q --upgrade pip
  pip install -q torch torchvision torchaudio --index-url https://download.pytorch.org/whl/cu128
  pip install -q \
    https://github.com/1CatAI/1Cat-vLLM/releases/download/v0.0.3/flash_attn_v100-26.2-cp312-cp312-linux_x86_64.whl \
    https://github.com/1CatAI/1Cat-vLLM/releases/download/v0.0.3/vllm-0.0.3.dev0+g72bb24e2d.d20260430.cu128-cp312-cp312-linux_x86_64.whl
else
  echo "==> vLLM venv exists"
fi

echo "==> cache model weights"
# shellcheck source=/dev/null
source "$VENV/bin/activate"
python3 - <<PY
from huggingface_hub import snapshot_download
path = snapshot_download("$MODEL")
print("cached:", path)
PY

chmod +x "$ROOT/scripts/singularity/"*.sh "$ROOT/scripts/singularity/"*.py

echo ""
echo "Bootstrap done. Start stack:"
echo "  $ROOT/scripts/singularity/start-vllm-pd.sh"
echo "  $ROOT/scripts/singularity/start-router.sh"
echo ""
echo "Optional systemd (requires sudo):"
echo "  sudo cp $ROOT/scripts/singularity/systemd/*.service /etc/systemd/system/"
echo "  sudo systemctl daemon-reload"
echo "  sudo systemctl enable --now demiurge-vllm-pd demiurge-router"
