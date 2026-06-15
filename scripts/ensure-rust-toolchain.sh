#!/usr/bin/env bash
# Install/update Rust to match rust-toolchain.toml (Cargo.lock v4 needs 1.83+).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MIN_RUST_VERSION="${DEMIURGE_MIN_RUST_VERSION:-1.83.0}"

bold() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

version_ge() {
  # True when $1 >= $2 (semver).
  [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]
}

if ! command -v rustup >/dev/null 2>&1; then
  bold "install rustup"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi

# shellcheck disable=SC1091
source "${HOME}/.cargo/env"

bold "sync toolchain from rust-toolchain.toml"
cd "$ROOT"
rustup update stable
rustup component add rustfmt clippy 2>/dev/null || true

active="$(rustc --version | awk '{print $2}')"
if ! version_ge "$active" "$MIN_RUST_VERSION"; then
  echo "ERROR: rustc $active is older than required $MIN_RUST_VERSION (Cargo.lock v4)" >&2
  echo "  run: rustup update stable" >&2
  exit 1
fi

echo "rustc $active (min $MIN_RUST_VERSION) — lockfile v4 OK"
