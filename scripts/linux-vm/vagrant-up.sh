#!/usr/bin/env bash
# One-shot headless Track B VM via Vagrant + VirtualBox.
#
#   ./scripts/linux-vm/vagrant-up.sh          # up + provision (~10 min first run)
#   ./scripts/linux-vm/vagrant-up.sh ssh    # shell into guest
#   ./scripts/linux-vm/vagrant-up.sh destroy
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

if ! command -v vagrant >/dev/null 2>&1; then
  echo "vagrant not found — install with: brew install vagrant" >&2
  exit 1
fi
if ! command -v VBoxManage >/dev/null 2>&1; then
  echo "VirtualBox not found — install with: brew install --cask virtualbox" >&2
  exit 1
fi

chmod +x vagrant-provision.sh 2>/dev/null || true

case "${1:-up}" in
  up)
    vagrant up --provider=virtualbox
    cat <<'EOF'

Track B VM is up (headless).

  vagrant ssh
  cd /demiurge && ./scripts/gate.sh
  ls target/bpf/admit_shed.o

From host: ./scripts/linux-vm/vagrant-up.sh ssh

EOF
    ;;
  ssh)
    vagrant ssh
    ;;
  provision | bootstrap)
    vagrant provision
    ;;
  halt)
    vagrant halt
    ;;
  destroy)
    vagrant destroy -f
    ;;
  status)
    vagrant status
    ;;
  *)
    echo "usage: $0 [up|ssh|provision|halt|destroy|status]" >&2
    exit 1
    ;;
esac
