#!/usr/bin/env bash
# Required Track B checks (Linux only): BPF compile + root XDP veth smoke.
set -euo pipefail
cd "$(dirname "$0")/.."

ROOT="$PWD"

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "track-b-gate: skip — not Linux (run ./scripts/gate.sh in the Track B VM)" >&2
  exit 0
fi

as_root() {
  if [[ "$(id -u)" -eq 0 ]]; then
    "$@"
  else
    # sudo resets PATH — keep the runner/toolchain cargo on root invocations.
    sudo env "PATH=$PATH" "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}" "HOME=$HOME" "$@"
  fi
}

bold "Track B gate — BPF object"
bash ./scripts/build-bpf.sh
test -f target/bpf/admit_shed.o

if ! command -v ping >/dev/null 2>&1; then
  bold "install iputils-ping (XDP veth smoke)"
  if command -v apt-get >/dev/null 2>&1; then
    as_root apt-get update -qq
    as_root apt-get install -y -qq iputils-ping
  else
    echo "ERROR: ping not found; install iputils-ping for XDP smoke" >&2
    exit 1
  fi
fi

bold "Track B gate — XDP veth smoke (root)"
export DEMIURGE_BPF_OBJECT="${DEMIURGE_BPF_OBJECT:-$ROOT/target/bpf/admit_shed.o}"
export DEMIURGE_XDP_FLAGS="${DEMIURGE_XDP_FLAGS:-skb}"
as_root env DEMIURGE_BPF_OBJECT="$DEMIURGE_BPF_OBJECT" DEMIURGE_XDP_FLAGS="$DEMIURGE_XDP_FLAGS" \
  ./scripts/xdp-veth-smoke.sh

printf '\n\033[1;32mTRACK B GATE: PASSED\033[0m\n'
