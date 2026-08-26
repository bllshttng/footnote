#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/cli"
uv sync --quiet
# x-71b6 In-N-Out tiering: some real subcommand trees are hidden from the
# curated `fno --help`, so probe the full-surface door `fno help --all` (which
# lists every command, hidden included). They stay invocable either way.
# The old list (state graph runtime worker event claim mail) was all moved
# spellings; d-26002be8 stopped rendering those anywhere, and the x-6233 fold
# retired runtime outright, so the door now exposes the hidden REAL roots.
out=$(uv run fno-py help --all 2>&1)
for name in do inbox update; do
  echo "$out" | grep -q "$name" || { echo "FAIL: help --all missing '$name'"; exit 1; }
done
echo "$out" | grep -qE '^  runtime' && { echo "FAIL: retired 'runtime' renders as a root"; exit 1; }
echo "PASS: help --all lists the hidden real roots, no retired spellings"
