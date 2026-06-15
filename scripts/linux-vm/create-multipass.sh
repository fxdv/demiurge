#!/usr/bin/env bash
# Headless Ubuntu VM alternative (no GUI) — often faster to bootstrap on Apple Silicon.
#
#   brew install multipass
#   ./scripts/linux-vm/create-multipass.sh
#
# SSH in: multipass shell demiurge-track-b
set -euo pipefail

VM_NAME="${VM_NAME:-demiurge-track-b}"
VM_CPUS="${VM_CPUS:-4}"
VM_MEM="${VM_MEM:-8G}"
VM_DISK="${VM_DISK:-40G}"
REPO="${DEMIURGE_REPO:-$(cd "$(dirname "$0")/../.." && pwd)}"
CLOUD_INIT="$(dirname "$0")/cloud-init.yaml"

if ! command -v multipass >/dev/null 2>&1; then
  echo "multipass not found — install with: brew install multipass" >&2
  exit 1
fi

if multipass info "$VM_NAME" >/dev/null 2>&1; then
  echo "Instance '$VM_NAME' already exists"
else
  multipass launch 24.04 \
    --name "$VM_NAME" \
    --cpus "$VM_CPUS" \
    --memory "$VM_MEM" \
    --disk "$VM_DISK" \
    --cloud-init "$CLOUD_INIT"
fi

if [[ -d "$REPO" ]]; then
  multipass mount "$REPO" "${VM_NAME}:/mnt/demiurge" 2>/dev/null || true
fi

IP="$(multipass info "$VM_NAME" | awk '/IPv4/ { print $2; exit }')"
cat <<EOF

Multipass instance '$VM_NAME' is up (${IP:-unknown}).

  multipass shell $VM_NAME
  cd /mnt/demiurge  # if mount succeeded, else: git clone …
  ./scripts/linux-vm/bootstrap-guest.sh /mnt/demiurge

EOF
