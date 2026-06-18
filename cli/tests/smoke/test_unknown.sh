#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/cli"
uv sync --quiet
set +e
out=$(uv run fno bogus 2>&1)
rc=$?
set -e
if [[ $rc -eq 0 ]]; then
  echo "FAIL: expected non-zero exit for unknown subcommand, got $rc"
  exit 1
fi
echo "$out" | grep -qi "no such command\|usage\|error" || {
  echo "FAIL: unknown-command error text missing"; echo "$out"; exit 1;
}
echo "PASS: unknown subcommand fails with hint"
