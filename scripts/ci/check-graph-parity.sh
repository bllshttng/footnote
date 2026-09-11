#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--self-test" ]]; then
  uv run --project cli python -m fno.graph.parity --self-test
else
  uv run --project cli fno doctor lint graph-parity
fi
