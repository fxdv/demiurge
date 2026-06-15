#!/usr/bin/env bash
# One-time Track B guest setup (run inside Ubuntu VM).
# Installs toolchain deps, Rust, builds BPF object, runs the full gate + Track B checks.
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "bootstrap-guest: run this inside the Linux VM" >&2
  exit 1
fi

REPO="${1:-${DEMIURGE_REPO:-$HOME/demiurge}}"
if [[ ! -f "$REPO/Cargo.toml" ]] || [[ ! -f "$REPO/scripts/gate.sh" ]]; then
  echo "bootstrap-guest: repo not found at $REPO" >&2
  echo "  expected Cargo.toml and scripts/gate.sh (Vagrant rsync excludes .git — that is OK)" >&2
  exit 1
fi

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

as_root() {
  if [[ "$(id -u)" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

bold "apt packages (BPF + build + Track B smoke)"
export DEBIAN_FRONTEND=noninteractive
as_root apt-get update
HEADER_PKG="linux-headers-$(uname -r)"
if ! apt-cache show "$HEADER_PKG" >/dev/null 2>&1; then
  HEADER_PKG=linux-headers-generic
fi
as_root apt-get install -y \
  build-essential \
  ca-certificates \
  clang \
  curl \
  git \
  iproute2 \
  iputils-ping \
  libbpf-dev \
  libssl-dev \
  llvm \
  pkg-config \
  python3 \
  rsync \
  "$HEADER_PKG"

bold "Rust toolchain (rust-toolchain.toml, Cargo.lock v4)"
bash "$REPO/scripts/ensure-rust-toolchain.sh"

cd "$REPO"
./scripts/bootstrap.sh

bold "full gate (CI mirror + required Track B gate)"
./scripts/gate.sh

printf '\n\033[1;32mTrack B guest ready\033[0m\n'
echo "Repo: $REPO"
echo "BPF:  $REPO/target/bpf/admit_shed.o"
echo ""
echo "Re-sync from Mac:  cd scripts/linux-vm && vagrant rsync"
echo "Full Track B report:  ./scripts/track-b-verify.sh  →  target/track-b-verify/report.md"
