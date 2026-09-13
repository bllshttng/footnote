#!/usr/bin/env bash
set -euo pipefail

# Build the worker first: the keeper every scenario spawns must carry the
# keeper-op surface under test (parity, import-on-open), not whatever an
# installed binary predates.
REPO_ROOT="$(git rev-parse --show-toplevel)"
cargo build --manifest-path "$REPO_ROOT/crates/fno-agents/Cargo.toml" --bin fno-agents-worker
export FNO_AGENTS_WORKER="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents-worker"

if [[ "${1:-}" == "--self-test" ]]; then
  # A pytest run is a more thorough self-test than the module could give itself
  # (8 scenarios vs. one fixture), so it replaces a --self-test CLI flag rather
  # than duplicating pytest's own coverage inside the shipped module.
  uv run --project cli python -m pytest cli/tests/unit/test_graph_parity.py -q
else
  uv run --project cli fno doctor lint graph-parity
fi
