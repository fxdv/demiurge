#!/usr/bin/env bash
# Build apt + rustup offline bundle on the Mac host (uses Docker; guest internet not required).
#
#   ./scripts/linux-vm/prepare-offline-bundle.sh
#   ./scripts/linux-vm/push-repo.sh
#   multipass shell demiurge-track-b
#   ~/demiurge/scripts/linux-vm/bootstrap-offline.sh ~/demiurge
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BUNDLE="$ROOT/target/linux-vm-bundle"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker not found — install Docker Desktop or fix guest networking instead" >&2
  exit 1
fi

mkdir -p "$BUNDLE/debs"

echo "Downloading Ubuntu arm64 .debs + rustup-init into $BUNDLE ..."
docker run --platform linux/arm64 --rm \
  -v "$BUNDLE:/bundle" \
  ubuntu:24.04 bash -eu -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y ca-certificates curl
    pkgs=(
      build-essential
      ca-certificates
      clang
      curl
      git
      iproute2
      libbpf-dev
      libssl-dev
      linux-headers-generic
      llvm
      pkg-config
    )
    cd /bundle/debs
    apt-get download "${pkgs[@]}"
    # Pull recursive runtime deps (best effort).
    apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts \
      --no-breaks --no-replaces --no-enhances "${pkgs[@]}" \
      | awk "/^[^ ]/ {print \$1}" | sort -u | while read -r dep; do
        apt-get download "$dep" 2>/dev/null || true
      done
    curl -fsSL -o /bundle/rustup-init https://static.rust-lang.org/rustup/dist/aarch64-unknown-linux-gnu/rustup-init
    chmod +x /bundle/rustup-init
    echo "bundle ready: $(find /bundle/debs -name "*.deb" | wc -l) debs"
  '

echo "Done. Transfer with push-repo.sh, then bootstrap-offline.sh inside the guest."
