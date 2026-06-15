#!/usr/bin/env bash
# Create a local VirtualBox Ubuntu VM for Track B (XDP / io_uring / gate.sh).
# Run on the Mac host after VirtualBox is installed.
#
# Usage:
#   brew install --cask virtualbox          # once; approve system extension in Settings
#   ./scripts/linux-vm/create-vbox.sh
#   ./scripts/linux-vm/create-vbox.sh --start
#
# After first boot, install Ubuntu from the attached ISO, then in the guest:
#   git clone https://github.com/fxdv/demiurge.git ~/demiurge
#   ./demiurge/scripts/linux-vm/bootstrap-guest.sh
set -euo pipefail

VM_NAME="${VM_NAME:-demiurge-track-b}"
VM_CPUS="${VM_CPUS:-4}"
VM_MEM_MB="${VM_MEM_MB:-8192}"
VM_DISK_GB="${VM_DISK_GB:-40}"
ISO_DIR="${ISO_DIR:-$HOME/VirtualBox/ISOs}"
REPO="${DEMIURGE_REPO:-$(cd "$(dirname "$0")/../.." && pwd)}"
START=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --start) START=1 ;;
    -h | --help)
      sed -n '2,20p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 1
      ;;
  esac
  shift
done

if ! command -v VBoxManage >/dev/null 2>&1; then
  cat >&2 <<'EOF'
VBoxManage not found.

Install VirtualBox (requires your password + System Settings approval):
  brew install --cask virtualbox

Then open System Settings → Privacy & Security and allow Oracle VirtualBox,
reboot if prompted, and re-run this script.
EOF
  exit 1
fi

HOST_ARCH="$(uname -m)"
UBUNTU_RELEASE="${UBUNTU_RELEASE:-24.04.4}"
case "$HOST_ARCH" in
  arm64)
    OSTYPE="Ubuntu_arm64"
    PLATFORM_ARCH="arm"
    ISO_NAME="ubuntu-${UBUNTU_RELEASE}-live-server-arm64.iso"
    ISO_URL="https://cdimage.ubuntu.com/releases/24.04/release/${ISO_NAME}"
    ;;
  x86_64)
    OSTYPE="Ubuntu_64"
    PLATFORM_ARCH="x86"
    ISO_NAME="ubuntu-${UBUNTU_RELEASE}-live-server-amd64.iso"
    ISO_URL="https://releases.ubuntu.com/noble/${ISO_NAME}"
    ;;
  *)
    echo "unsupported host arch: $HOST_ARCH" >&2
    exit 1
    ;;
esac

if [[ -n "${UBUNTU_ISO_URL:-}" ]]; then
  ISO_URL="$UBUNTU_ISO_URL"
fi

mkdir -p "$ISO_DIR"
ISO_PATH="$ISO_DIR/$ISO_NAME"
if [[ ! -f "$ISO_PATH" ]]; then
  echo "Checking $ISO_URL ..."
  if ! curl -fsI "$ISO_URL" >/dev/null; then
    echo "ERROR: ISO not found at $ISO_URL" >&2
    echo "Try UBUNTU_RELEASE=24.04.3 or set UBUNTU_ISO_URL to a valid .iso URL" >&2
    exit 1
  fi
  echo "Downloading $ISO_URL (~2.8 GB) ..."
  curl -fL --progress-bar -o "$ISO_PATH.part" "$ISO_URL"
  mv "$ISO_PATH.part" "$ISO_PATH"
fi

if VBoxManage list vms | grep -q "\"${VM_NAME}\""; then
  echo "VM '$VM_NAME' already exists — skipping create (use VBoxManage unregistervm to remove)"
else
  DISK_DIR="$HOME/VirtualBox VMs/$VM_NAME"
  mkdir -p "$DISK_DIR"
  DISK="$DISK_DIR/${VM_NAME}.vdi"

  echo "Creating VM '$VM_NAME' ($OSTYPE, ${VM_CPUS} vCPU, ${VM_MEM_MB} MiB RAM, ${VM_DISK_GB} GiB disk)"
  VBoxManage createvm --name "$VM_NAME" --ostype "$OSTYPE" --platform-architecture "$PLATFORM_ARCH" --register
  VBoxManage modifyvm "$VM_NAME" \
    --memory "$VM_MEM_MB" \
    --cpus "$VM_CPUS" \
    --graphicscontroller vmsvga \
    --firmware efi \
    --nic1 nat \
    --audio none \
    --usb off

  VBoxManage storagectl "$VM_NAME" --name SATA --add sata --controller IntelAhci
  VBoxManage createmedium disk --filename "$DISK" --size "$((VM_DISK_GB * 1024))" --format VDI
  VBoxManage storageattach "$VM_NAME" --storagectl SATA --port 0 --device 0 --type hdd --medium "$DISK"
  VBoxManage storageattach "$VM_NAME" --storagectl SATA --port 1 --device 0 --type dvddrive --medium "$ISO_PATH"
  VBoxManage modifyvm "$VM_NAME" --boot1 dvd --boot2 disk --boot3 none --boot4 none
fi

if [[ -d "$REPO" ]]; then
  if ! VBoxManage showvminfo "$VM_NAME" --machinereadable | grep -q 'SharedFolderNameMachine.*demiurge'; then
    VBoxManage sharedfolder add "$VM_NAME" --name demiurge --hostpath "$REPO" --automount
    echo "Shared folder: host $REPO → guest /media/sf_demiurge (after Guest Additions)"
  fi
fi

cat <<EOF

VM '$VM_NAME' is ready.

1. Start the VM (VirtualBox GUI or: VBoxManage startvm "$VM_NAME" --type gui)
2. Install Ubuntu Server from the attached ISO (use defaults; enable OpenSSH if prompted)
3. In the guest, either:
     git clone https://github.com/fxdv/demiurge.git ~/demiurge
   or mount the shared folder (requires Guest Additions):
     sudo usermod -aG vboxsf "\$USER" && newgrp vboxsf
     ls /media/sf_demiurge
4. Run: ~/demiurge/scripts/linux-vm/bootstrap-guest.sh

Note ($(uname -m) host): Track B CI targets linux x86_64 release binaries.
An ARM64 guest is fine for BPF compile, io_uring dev, and gate.sh; use x86_64
Linux (cloud VM or Intel Mac VBox) for production NIC / x86 perf characterization.

EOF

if [[ "$START" -eq 1 ]]; then
  VBoxManage startvm "$VM_NAME" --type gui
fi
