#!/usr/bin/env bash
# Upload publish artifact files to an existing GitHub Release (skips missing optional files).
set -euo pipefail

TAG="${1:?usage: gh-release-upload.sh <tag>}"
# shellcheck disable=SC1091
source "${PUBLISH_ENV:-target/release-artifacts/publish.env}"

files=("$TARBALL" "$STAGING/RELEASE-one-pager.md" "$STAGING/load-bench/latest.pseudo")
optional=("$STAGING/load-bench/stress.pseudo")
for f in "${optional[@]}"; do
  if [[ -f "$f" ]]; then
    files+=("$f")
  fi
done

gh release upload "$TAG" "${files[@]}" --clobber
