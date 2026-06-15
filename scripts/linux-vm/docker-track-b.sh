#!/usr/bin/env bash
# Track B Linux environment via Docker (reliable on macOS when Multipass networking fails).
#
#   ./scripts/linux-vm/docker-track-b.sh bootstrap   # first time (~5–10 min)
#   ./scripts/linux-vm/docker-track-b.sh shell       # interactive Linux shell
#   ./scripts/linux-vm/docker-track-b.sh gate        # re-run gate only
#   ./scripts/linux-vm/docker-track-b.sh bpf         # build BPF object only
#   ./scripts/linux-vm/docker-track-b.sh down        # remove container
set -euo pipefail

CONTAINER="${CONTAINER:-demiurge-track-b}"
IMAGE="${IMAGE:-ubuntu:24.04}"
REPO="${DEMIURGE_REPO:-$(cd "$(dirname "$0")/../.." && pwd)}"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker not found — install Docker Desktop" >&2
  exit 1
fi

docker_platform() {
  case "$(uname -m)" in
    arm64) echo linux/arm64 ;;
    x86_64) echo linux/amd64 ;;
    *) echo "unsupported host: $(uname -m)" >&2; exit 1 ;;
  esac
}

ensure_container() {
  local platform
  platform="$(docker_platform)"
  if docker ps -a --format '{{.Names}}' | grep -qx "$CONTAINER"; then
    if ! docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
      docker start "$CONTAINER" >/dev/null
    fi
  else
    docker volume create "${CONTAINER}-target" >/dev/null 2>&1 || true
    docker volume create "${CONTAINER}-cargo" >/dev/null 2>&1 || true
    docker run -d --platform "$platform" --name "$CONTAINER" \
      --privileged \
      -v "$REPO:/work" \
      -v "${CONTAINER}-target:/work/target" \
      -v "${CONTAINER}-cargo:/root/.cargo" \
      -w /work \
      "$IMAGE" sleep infinity >/dev/null
  fi
}

run_in_container() {
  ensure_container
  docker exec -it "$CONTAINER" bash -lc "$*"
}

run_in_container_batch() {
  ensure_container
  docker exec "$CONTAINER" bash -lc "$*"
}

cmd="${1:-help}"
shift || true

case "$cmd" in
  bootstrap)
    echo "Track B bootstrap in Docker ($CONTAINER) — repo mounted at /work"
    run_in_container_batch '/work/scripts/linux-vm/bootstrap-guest.sh /work'
    ;;
  shell)
    ensure_container
    echo "Linux shell — repo at /work (exit to leave container running)"
    docker exec -it "$CONTAINER" bash -lc 'cd /work && exec bash'
    ;;
  gate)
    run_in_container_batch 'cd /work && ./scripts/gate.sh'
    ;;
  bpf)
    run_in_container_batch 'cd /work && bash ./scripts/build-bpf.sh'
    ;;
  down)
    docker rm -f "$CONTAINER" 2>/dev/null || true
    echo "removed $CONTAINER (volumes ${CONTAINER}-target / ${CONTAINER}-cargo kept for cache)"
    ;;
  purge)
    docker rm -f "$CONTAINER" 2>/dev/null || true
    docker volume rm "${CONTAINER}-target" "${CONTAINER}-cargo" 2>/dev/null || true
    echo "removed $CONTAINER and build caches"
    ;;
  logs | status)
    docker ps -a --filter "name=^${CONTAINER}$"
    ;;
  help | *)
    sed -n '2,12p' "$0"
    ;;
esac
