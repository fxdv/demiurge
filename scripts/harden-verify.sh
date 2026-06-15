#!/usr/bin/env bash
# Die-hard verify (Tiers 1–4) — wrapper around scripts/verify/harden-all.sh.
set -euo pipefail
exec "$(dirname "$0")/verify/harden-all.sh" "$@"
