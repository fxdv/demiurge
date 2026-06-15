#!/usr/bin/env bash
# Copy the host repo into the Multipass guest (no sshfs mount required).
#
#   ./scripts/linux-vm/push-repo.sh
#   multipass shell demiurge-track-b -- ./demiurge/scripts/linux-vm/bootstrap-guest.sh ~/demiurge
set -euo pipefail

VM_NAME="${VM_NAME:-demiurge-track-b}"
REPO="${DEMIURGE_REPO:-$(cd "$(dirname "$0")/../.." && pwd)}"
GUEST_PATH="${GUEST_PATH:-/home/ubuntu/demiurge}"

if ! multipass info "$VM_NAME" >/dev/null 2>&1; then
  echo "instance '$VM_NAME' not found" >&2
  exit 1
fi

echo "Transferring $REPO → ${VM_NAME}:${GUEST_PATH} (excludes target/, .git/objects if rsync unavailable)"
multipass exec "$VM_NAME" -- mkdir -p "$GUEST_PATH"

# multipass transfer has no exclude flag — use tar on host to skip heavy dirs.
tar -C "$REPO" \
  --exclude=target \
  --exclude=.git/objects/pack \
  -cf - . | multipass exec "$VM_NAME" -- tar -C "$GUEST_PATH" -xf -

echo "Done. In guest:"
echo "  multipass shell $VM_NAME"
echo "  ./scripts/linux-vm/bootstrap-guest.sh $GUEST_PATH"
