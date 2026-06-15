#!/usr/bin/env bash
# Fix common Multipass "no internet" issues on macOS.
#
# Run on the Mac host (needs sudo for firewall step):
#   ./scripts/linux-vm/fix-multipass-network.sh
#
# Then in the guest verify:
#   ping -c2 1.1.1.1
#   ping -c2 github.com
set -euo pipefail

VM_NAME="${VM_NAME:-demiurge-track-b}"

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

if ! command -v multipass >/dev/null 2>&1; then
  echo "multipass not found" >&2
  exit 1
fi

bold "1/3 macOS firewall — allow bootpd (DHCP/NAT; needs your password)"
if sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add /usr/libexec/bootpd 2>/dev/null; then
  sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblock /usr/libexec/bootpd
  echo "bootpd whitelisted"
else
  echo "Could not update firewall (run manually with sudo):" >&2
  echo "  sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add /usr/libexec/bootpd" >&2
  echo "  sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblock /usr/libexec/bootpd" >&2
fi

bold "2/3 restart instance"
multipass restart "$VM_NAME"
sleep 5

bold "3/3 guest DNS + route check"
if multipass exec "$VM_NAME" -- bash -lc '
  set -e
  echo "--- ip route ---"
  ip route
  echo "--- resolv.conf ---"
  cat /etc/resolv.conf
  echo "--- ping 1.1.1.1 (routing) ---"
  ping -c2 -W3 1.1.1.1
  echo "--- fix DNS (public resolvers) ---"
  sudo resolvectl dns eth0 1.1.1.1 8.8.8.8 2>/dev/null || {
    echo "nameserver 1.1.1.1" | sudo tee /etc/resolv.conf >/dev/null
    echo "nameserver 8.8.8.8" | sudo tee -a /etc/resolv.conf >/dev/null
  }
  echo "--- ping github.com (DNS) ---"
  ping -c2 -W3 github.com
  echo "--- curl github ---"
  curl -fsI --max-time 10 https://github.com | head -1
'; then
  echo ""
  echo "Guest internet looks OK. Continue with:"
  echo "  git clone https://github.com/fxdv/demiurge.git ~/demiurge   # in guest"
  echo "  ~/demiurge/scripts/linux-vm/bootstrap-guest.sh ~/demiurge"
else
  cat >&2 <<EOF

Guest still cannot reach the internet.

Try bridged networking (VM uses your Wi‑Fi/LAN directly — often fixes VPN DNS issues):

  multipass delete $VM_NAME --purge
  multipass set local.bridged-network=en0
  cd $(cd "$(dirname "$0")/../.." && pwd)
  multipass launch 24.04 --name $VM_NAME --cpus 4 --memory 8G --disk 40G --bridged \\
    --cloud-init scripts/linux-vm/cloud-init.yaml

Or stay offline and push a bundle from the host:

  ./scripts/linux-vm/prepare-offline-bundle.sh
  ./scripts/linux-vm/push-repo.sh
  multipass shell $VM_NAME
  ~/demiurge/scripts/linux-vm/bootstrap-offline.sh ~/demiurge

EOF
  exit 1
fi
