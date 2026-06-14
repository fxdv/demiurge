#!/usr/bin/env bash
# Compile bpf/admit_shed.bpf.c → target/bpf/admit_shed.o (Linux + clang only).
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "build-bpf: skip — XDP object build requires Linux (clang -target bpf)" >&2
  exit 0
fi

if ! command -v clang >/dev/null 2>&1; then
  echo "build-bpf: clang not found" >&2
  exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) BPF_ARCH=x86 ;;
  aarch64) BPF_ARCH=arm64 ;;
  *)
    echo "build-bpf: unsupported arch $ARCH" >&2
    exit 1
    ;;
esac

INCLUDE=""
if [[ -d "/usr/include/${ARCH}-linux-gnu" ]]; then
  INCLUDE="-I/usr/include/${ARCH}-linux-gnu"
fi

mkdir -p target/bpf
clang -O2 -g -Wall -Werror \
  -target bpf \
  -D__TARGET_ARCH_${BPF_ARCH} \
  ${INCLUDE} \
  -c bpf/admit_shed.bpf.c \
  -o target/bpf/admit_shed.o

echo "build-bpf: OK → target/bpf/admit_shed.o ($(wc -c < target/bpf/admit_shed.o) bytes)"
