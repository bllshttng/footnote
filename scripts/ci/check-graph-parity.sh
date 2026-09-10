#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--self-test" ]]; then
  uv run --project cli python scripts/analysis/graph-parity.py --self-test
else
  uv run --project cli fno doctor lint graph-parity
fi
