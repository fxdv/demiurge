#!/usr/bin/env bash
# Runtime XDP smoke on a veth pair (Track B — run inside Vagrant/Linux as root).
#
#   sudo ./scripts/xdp-veth-smoke.sh
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "xdp-veth-smoke: Linux only" >&2
  exit 1
fi
if [[ "$(id -u)" -ne 0 ]]; then
  echo "xdp-veth-smoke: run as root (sudo)" >&2
  exit 1
fi

bash ./scripts/build-bpf.sh
export DEMIURGE_BPF_OBJECT="$(pwd)/target/bpf/admit_shed.o"
export DEMIURGE_XDP_FLAGS=skb
cargo test -p demiurge-dataplane --test xdp_veth -- --ignored --nocapture
echo "xdp-veth-smoke: OK"
