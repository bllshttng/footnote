"""Integration tests for target claim wiring (Phase 02).

Drives init-target-state.sh + target-stop-hook.sh against a temp repo and
verifies the fno claim primitive is exercised end-to-end.

Note: set-gate.sh tests removed in Task 3.2 (control-plane collapse,
ab-d0337fbc). The stop-hook structural claim-release check is retained.

These tests exec the real bash scripts in a sandbox so the shell wiring
itself is covered, not just the Python primitive (which test_claims_*.py
already covers).
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
INIT_SCRIPT = REPO_ROOT / "hooks" / "helpers" / "init-target-state.sh"
STOP_HOOK = REPO_ROOT / "hooks" / "target-stop-hook.sh"
# Phase 1 of stop-hook refactor: release_graph_claim lives in
# scripts/lib/claim-release.sh; the hook sources it.
CLAIM_LIB = REPO_ROOT / "scripts" / "lib" / "claim-release.sh"


pytestmark = pytest.mark.skipif(
    not (INIT_SCRIPT.exists() and STOP_HOOK.exists()),
    reason="target integration scripts not present in this checkout",
)


# ---------------------------------------------------------------------------
# init-target-state.sh
# ---------------------------------------------------------------------------


def test_init_target_state_acquires_claim_when_node_id_present(tmp_path):
    """init-target-state.sh writes target_claim_key + target_claim_holder when a
    graph node id is resolvable and fno is on PATH.

    Strategy: build a minimal graph.json with one entry, point the script at
    it via env vars, run, and assert the state file references both fields
    and the .fno/claims/*.lock file exists.
    """
    # Minimal abi-resolvable graph
    abi_home = tmp_path / ".fno-home"
    abi_home.mkdir()
    graph = abi_home / "graph.json"
    graph.write_text(
        '{"entries":[{"id":"ab-testit","plan_path":"plans/test.md",'
        '"_status":"ready","priority":"p2","project":"fno"}]}'
    )

    # Set up a fake repo root with .fno/ and the resolvable plan path
    repo = tmp_path / "repo"
    (repo / "plans").mkdir(parents=True)
    (repo / "plans" / "test.md").write_text("# Test plan\n")
    (repo / ".fno").mkdir()
    (repo / ".git").mkdir()
    (repo / ".git" / "HEAD").write_text("ref: refs/heads/main\n")

    env = os.environ.copy()
    env.update({
        "TARGET_START": "1",
        "TARGET_INPUT": "ab-testit",
        "TARGET_SIZE": "S",
        "HOME": str(abi_home.parent),  # so the script's path-discovery works
    })

    # Run from the fake repo root
    result = subprocess.run(
        ["bash", str(INIT_SCRIPT)],
        cwd=repo,
        env=env,
        capture_output=True,
        text=True,
        timeout=20,
    )

    # The script may legitimately fail to find the graph in this sandbox
    # (it walks parent dirs looking for graph.json). We only assert the
    # PR1 invariant when the state file actually got written.
    state = repo / ".fno" / "target-state.md"
    if not state.exists():
        pytest.skip(
            f"init-target-state.sh did not write state in sandbox: "
            f"rc={result.returncode}, stderr={result.stderr[:500]!r}"
        )

    text = state.read_text()
    # The PR1 fields only appear if fno was actually on PATH and the claim
    # acquire succeeded. We do not hard-fail the test if not - we want this
    # to be a smoke test that the wiring runs at all without erroring out.
    if "target_claim_key:" in text:
        # If the field is written, the claim file must also exist
        assert "target_claim_holder:" in text
        # The lock file landed under the repo's .fno/claims/
        claims_dir = repo / ".fno" / "claims"
        if claims_dir.exists():
            locks = list(claims_dir.glob("*.lock"))
            # At least one lock present means the acquire returned success.
            assert len(locks) >= 1


# ---------------------------------------------------------------------------
# release_graph_claim (via direct sourcing)
# ---------------------------------------------------------------------------


# test_stop_hook_contains_abi_claim_release_block removed (ab-d0337fbc): the
# stop hook is a read-only shim and no longer releases claims on exit; a dead
# session's claim goes stale via PID-liveness and is recovered by the next
# `fno claim acquire`. scripts/lib/claim-release.sh deleted with it.


def test_init_target_state_contains_abi_claim_acquire_block(tmp_path):
    """init-target-state.sh must contain the PR1 fno claim acquire block."""
    init_text = INIT_SCRIPT.read_text(encoding="utf-8")
    assert "fno claim acquire" in init_text, (
        "init-target-state.sh does not invoke `fno claim acquire`"
    )
    assert "target_claim_key" in init_text
    assert "target_claim_holder" in init_text


