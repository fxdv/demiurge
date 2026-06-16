#!/usr/bin/env bash
# Shared console UI for Demiurge orchestrator scripts (gate, verify, load bench).
# Source from scripts:  source "$(dirname "$0")/lib/ui.sh"
#
#   DEMIURGE_BANNER=1  force banner even when stdout is not a TTY (e.g. tee)
#   DEMIURGE_BANNER=0  never print banner
#   DEMIURGE_UI_WIDTH=120  console box width (default 120)

DEMIURGE_UI_WIDTH="${DEMIURGE_UI_WIDTH:-120}"

_ui_should_banner() {
  case "${DEMIURGE_BANNER:-}" in
    1 | true | yes) return 0 ;;
    0 | false | no) return 1 ;;
  esac
  [[ -t 1 ]]
}

_ui_git_ref() {
  if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    printf '%s@%s' "$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')" \
      "$(git rev-parse --short HEAD 2>/dev/null || echo '?')"
  else
    printf 'unknown'
  fi
}

_ui_host_tag() {
  printf '%s · %s' "$(uname -s | tr '[:upper:]' '[:lower:]')" "$(uname -m)"
}

_ui_repeat() {
  local char="$1"
  local count="$2"
  printf '%*s' "$count" '' | tr ' ' "$char"
}

# demiurge_banner "TITLE" "line1" "line2" ...
demiurge_banner() {
  _ui_should_banner || return 0
  local title="$1"
  shift
  local w="$DEMIURGE_UI_WIDTH"
  local inner=$((w - 4))
  local rule
  rule="$(_ui_repeat '═' $((w - 2)))"

  printf '\n╔%s╗\n' "$rule"
  printf '║ %-*s ║\n' "$inner" "$title"
  if (("$#" > 0)); then
    printf '╠%s╣\n' "$rule"
  fi
  local line
  for line in "$@"; do
    if ((${#line} > inner)); then
      line="${line:0:$((inner - 1))}…"
    fi
    printf '║ %-*s ║\n' "$inner" "$line"
  done
  printf '╚%s╝\n\n' "$rule"
}

bold() {
  printf '\n\033[1m==> %s\033[0m\n' "$1"
}

demiurge_pass() {
  printf '\n\033[1;32m%s\033[0m\n' "$1"
}

demiurge_fail() {
  printf '\n\033[1;31m%s\033[0m\n' "$1" >&2
}
