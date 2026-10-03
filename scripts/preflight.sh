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

# Python for the CLI: python3 >= 3.11, or uv, which provisions its own.
if command -v python3 >/dev/null 2>&1; then
  if python3 -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 11) else 1)' 2>/dev/null; then
    echo "[ok] python3 (>= 3.11)"
  elif command -v uv >/dev/null 2>&1; then
    echo "[ok] uv (the python3 on PATH is older than 3.11; uv provisions its own)"
  else
    echo "[missing] python3 >= 3.11 (the python3 on PATH is older, and uv is not installed)"
    MISSING=1
  fi
elif command -v uv >/dev/null 2>&1; then
  echo "[ok] uv (no python3; uv provisions its own)"
else
  echo "[missing] python3 >= 3.11 or uv (install either)"
  MISSING=1
fi

# Cargo builds the Rust front door from source; every other channel ships it.
if command -v cargo >/dev/null 2>&1; then
  echo "[ok] cargo"
else
  echo "[optional] cargo (only the cargo install channel needs it; every other channel ships the binaries)"
fi

if [[ "$MISSING" -ne 0 ]]; then
  echo "Preflight failed: missing required dependencies" >&2
  exit 1
fi

# Presence is not a login: pushes and the target loop's PR reads fail on an
# unauthenticated gh. A clean runner (and the install-channel smoke) never
# logs in, so this warns instead of failing; the target loop itself parks
# with the same instructions when its PR read hits it.
if gh auth status >/dev/null 2>&1; then
  echo "[ok] gh auth"
else
  echo "[warn] gh is present but not authenticated; pushes and PR reads will fail" >&2
  echo "  Run: gh auth login" >&2
  echo "  If pushes still fail afterwards, also run: gh auth setup-git" >&2
fi

echo "Preflight passed"
