#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/cli"
uv sync --quiet
out=$(uv run fno --help 2>&1)
# `gate` removed from the probe list: the fno gate sub-app was deleted by
# the control-plane collapse wedge (ab-d0337fbc); `claim` probes in its place.
for name in state graph runtime worker event claim reality-check; do
  echo "$out" | grep -q "$name" || { echo "FAIL: --help missing '$name'"; exit 1; }
done
echo "PASS: --help lists seven subcommand trees"
