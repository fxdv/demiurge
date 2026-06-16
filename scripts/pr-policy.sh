#!/usr/bin/env bash
# PR policy — enforces CONTRIBUTING same-PR coupling + CLA acknowledgment.
# CI: Gate workflow · Policy job (pull_request only).
set -euo pipefail
cd "$(dirname "$0")/.."

BASE="${1:?usage: pr-policy.sh <base-ref>  e.g. origin/main}"

# shellcheck source=lib/ui.sh
source "$(dirname "$0")/lib/ui.sh"

if ! git rev-parse "$BASE" >/dev/null 2>&1; then
  git fetch origin "${BASE#origin/}" --depth=64 2>/dev/null || true
fi

if ! git merge-base "$BASE" HEAD >/dev/null 2>&1; then
  echo "POLICY FAIL: cannot diff against $BASE" >&2
  exit 1
fi

mapfile -t FILES < <(git diff --name-only "${BASE}...HEAD")
ERRORS=0

has_file() {
  local pattern="$1"
  local f
  for f in "${FILES[@]}"; do
    [[ "$f" =~ $pattern ]] && return 0
  done
  return 1
}

policy_fail() {
  echo "POLICY FAIL: $*" >&2
  ERRORS=$((ERRORS + 1))
}

demiurge_banner "PR policy" \
  "base    $BASE" \
  "files   ${#FILES[@]} changed"

# Same-PR rule: canonical params → regenerated artifacts.
if has_file '^design/demiurge\.params\.toml$'; then
  if ! has_file '^crates/demiurge-cost/src/generated_params\.rs$' \
    && ! has_file '^spec/generated/params_table\.tex$'; then
    policy_fail "design/demiurge.params.toml changed — run \`cargo xtask gen\` and commit generated outputs"
  fi
fi

# Same-PR rule: requirements → spec contract.
if has_file '^design/requirements\.toml$'; then
  if ! has_file '^spec/'; then
    policy_fail "design/requirements.toml changed — update spec/ in the same PR"
  fi
fi

# CLA (PR template checkbox or explicit sign statement).
if [[ -n "${GITHUB_EVENT_PATH:-}" && -f "$GITHUB_EVENT_PATH" ]]; then
  body=$(jq -r '.pull_request.body // ""' "$GITHUB_EVENT_PATH")
  if ! echo "$body" | grep -qiE '\[x\][[:space:]]*.*CLA|I sign.*CLA|read.*CLA\.md|Contributor License Agreement'; then
    policy_fail "PR body must acknowledge CLA (check the template box or state you sign CLA.md)"
  fi
fi

if [[ "$ERRORS" -gt 0 ]]; then
  echo "" >&2
  echo "$ERRORS PR policy check(s) failed — see CONTRIBUTING.md same-PR rule." >&2
  exit 1
fi

demiurge_pass "PR POLICY PASSED"
