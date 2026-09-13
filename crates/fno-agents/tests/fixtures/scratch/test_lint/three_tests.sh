#!/usr/bin/env bash
# Are the three fresh-HOME failures caused by this branch, or by the fresh HOME?
# Same tree, same tests, two HOMEs. If they fail only under the fresh HOME, the
# branch is not the cause.
set -uo pipefail
cd ~/code/footnote/footnote/.claude/worktrees/NODEID/cli || exit 2

T="tests/hooks/test_init_target_state_skip_flags.py::test_env_grant_scrubbed_on_agent_origin_run \
tests/hooks/test_init_target_state_skip_flags.py::test_immutable_manifest_has_no_mutable_fields \
tests/integration/test_target_node_claim.py::test_codex_thread_identity_aligns_manifest_graph_and_claim"

echo "=== normal HOME ==="
.venv/bin/python -m pytest -q $T -p no:cacheprovider 2>&1 | tail -4

echo
echo "=== fresh HOME ==="
FRESH="$(mktemp -d)"
HOME="$FRESH" .venv/bin/python -m pytest -q $T -p no:cacheprovider 2>&1 | tail -4
rm -rf "$FRESH"
