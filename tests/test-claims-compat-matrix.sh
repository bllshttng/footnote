#!/usr/bin/env bash
# The cross-impl claims compat matrix: the merge gate proving the Python claims
# library and the fno-agents claims module agree on every claim file. It must
# run, never skip. test_claims_cross_impl.py skips when find_dev_binary() finds
# no crates/fno-agents/target/debug/fno-agents or target/release build, so this
# harness asks the same function first and fails when it answers none.
# Discovery runs it after the smoke build step.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../cli"

if ! uv run python -c 'import sys; from fno.rust_binary import find_dev_binary; sys.exit(0 if find_dev_binary() else 1)'; then
  echo "test-claims-compat-matrix: no fno-agents dev build; run: cargo build --manifest-path crates/fno-agents/Cargo.toml" >&2
  exit 1
fi

uv run pytest --tb=short -q tests/integration/test_claims_cross_impl.py
