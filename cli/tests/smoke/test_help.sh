#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/cli"
uv sync --quiet
# x-71b6 In-N-Out tiering: these subcommand trees are hidden from the curated
# `fno --help`, so probe the full-surface door `fno help --all` (which lists
# every command, hidden included). They stay invocable either way.
out=$(uv run fno-py help --all 2>&1)
for name in state graph runtime worker event claim reality-check; do
  echo "$out" | grep -q "$name" || { echo "FAIL: help --all missing '$name'"; exit 1; }
done
echo "PASS: help --all lists seven subcommand trees"
