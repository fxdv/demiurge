#!/usr/bin/env bash
# Bootstrap Track B guest without internet (after prepare-offline-bundle.sh + push-repo.sh).
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "bootstrap-offline: run inside the Multipass guest" >&2
  exit 1
fi

REPO="${1:-${DEMIURGE_REPO:-$HOME/demiurge}}"
BUNDLE="$REPO/target/linux-vm-bundle"

if [[ ! -d "$REPO/.git" ]]; then
  echo "repo not found: $REPO" >&2
  exit 1
fi
if [[ ! -d "$BUNDLE/debs" ]] || [[ ! -x "$BUNDLE/rustup-init" ]]; then
  echo "offline bundle missing — on Mac host run: ./scripts/linux-vm/prepare-offline-bundle.sh" >&2
  exit 1
fi

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

bold "install .debs from bundle"
sudo dpkg -i "$BUNDLE/debs"/*.deb 2>/dev/null || true
# May still need one online pass for broken deps; ignore if already satisfied.
sudo apt-get -f install -y 2>/dev/null || true

if ! command -v rustup >/dev/null 2>&1; then
  bold "rustup (offline installer)"
  "$BUNDLE/rustup-init" -y --default-toolchain stable --no-modify-path
fi
bash "$REPO/scripts/ensure-rust-toolchain.sh"

cd "$REPO"
./scripts/bootstrap.sh

bold "BPF object"
bash ./scripts/build-bpf.sh
test -f target/bpf/admit_shed.o

bold "full gate"
./scripts/gate.sh

printf '\n\033[1;32mTrack B guest ready (offline bootstrap)\033[0m\n'
