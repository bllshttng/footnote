#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--self-test" ]]; then
  # A pytest run is a more thorough self-test than the module could give itself
  # (8 scenarios vs. one fixture), so it replaces a --self-test CLI flag rather
  # than duplicating pytest's own coverage inside the shipped module.
  uv run --project cli python -m pytest cli/tests/unit/test_graph_parity.py -q
else
  uv run --project cli fno doctor lint graph-parity
fi
