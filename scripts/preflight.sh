#!/usr/bin/env bash
set -euo pipefail

MISSING=0
for dep in bash git gh jq; do
  if command -v "$dep" >/dev/null 2>&1; then
    echo "[ok] $dep"
  else
    echo "[missing] $dep"
    MISSING=1
  fi
done

if [[ "$MISSING" -ne 0 ]]; then
  echo "Preflight failed: missing required dependencies" >&2
  exit 1
fi

echo "Preflight passed"
