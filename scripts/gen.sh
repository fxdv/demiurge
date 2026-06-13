#!/usr/bin/env bash
# Regenerate every artifact derived from the canonical inputs in design/.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo xtask gen
