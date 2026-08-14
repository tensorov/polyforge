#!/usr/bin/env bash
# T4 parity guard: the local reusable copy .github/actions/polyforge must stay
# byte-identical to the published tensorov/polyforge-action@v1 (canonical
# source). Run from the repo root; CI runs it in the parity job.
set -euo pipefail

REPO_URL="https://raw.githubusercontent.com/tensorov/polyforge-action/v1"
LOCAL_DIR=".github/actions/polyforge"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

for f in action.yml entrypoint.sh; do
  curl -fsSL "$REPO_URL/$f" -o "$tmp/$f"
done

if ! diff -u "$LOCAL_DIR/action.yml" "$tmp/action.yml" || ! diff -u "$LOCAL_DIR/entrypoint.sh" "$tmp/entrypoint.sh"; then
  echo "::error::local action copy drifted from published v1 — sync or bump version" >&2
  exit 1
fi

echo "parity OK: local action copy matches tensorov/polyforge-action@v1"