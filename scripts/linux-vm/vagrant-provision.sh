#!/usr/bin/env bash
# Vagrant shell provisioner — idempotent Track B bootstrap.
set -euo pipefail

REPO="${DEMIURGE_REPO:-/demiurge}"

required=(
  design/fleet-pilot.toml
  design/traces/synthetic-fleet.jsonl
)
for f in "${required[@]}"; do
  if [[ ! -f "$REPO/$f" ]]; then
    echo "vagrant-provision: missing $REPO/$f" >&2
    echo "  on host: git pull, then vagrant rsync-auto && vagrant provision" >&2
    exit 1
  fi
done

exec bash "$REPO/scripts/linux-vm/bootstrap-guest.sh" "$REPO"
