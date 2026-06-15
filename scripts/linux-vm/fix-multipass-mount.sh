#!/usr/bin/env bash
# Enable multipass mount (sshfs) when host-side auto-install fails.
#
#   ./scripts/linux-vm/fix-multipass-mount.sh
#   multipass mount "$DEMIURGE_REPO" demiurge-track-b:/mnt/demiurge
set -euo pipefail

VM_NAME="${VM_NAME:-demiurge-track-b}"
REPO="${DEMIURGE_REPO:-$(cd "$(dirname "$0")/../.." && pwd)}"

if ! command -v multipass >/dev/null 2>&1; then
  echo "multipass not found" >&2
  exit 1
fi

if ! multipass info "$VM_NAME" >/dev/null 2>&1; then
  echo "instance '$VM_NAME' not found — run ./create-multipass.sh first" >&2
  exit 1
fi

echo "Installing multipass-sshfs inside '$VM_NAME' (needs guest internet → snap store) ..."
if ! multipass exec "$VM_NAME" -- sudo snap install multipass-sshfs; then
  cat >&2 <<'EOF'
snap install failed inside the guest.

Workarounds (pick one):
  A) Clone in the guest (simplest):
       multipass shell demiurge-track-b
       git clone https://github.com/fxdv/demiurge.git ~/demiurge
       ~/demiurge/scripts/linux-vm/bootstrap-guest.sh ~/demiurge

  B) Copy host tree without mount (slower, includes local changes):
       ./scripts/linux-vm/push-repo.sh

  C) Fix guest DNS/network, then retry:
       multipass shell demiurge-track-b
       ping -c1 api.snapcraft.io
       sudo snap install multipass-sshfs
EOF
  exit 1
fi

echo "Retrying mount: $REPO → /mnt/demiurge"
multipass mount "$REPO" "${VM_NAME}:/mnt/demiurge"
multipass info "$VM_NAME" | grep -A1 '^Mounts:'
