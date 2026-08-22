"""Integration tests for target claim wiring (Phase 02).

Drives init-target-state.sh + target-stop-hook.sh against a temp repo and
verifies the fno agents claim primitive is exercised end-to-end.

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

# NO module-level skipif, and the omission is load-bearing. It used to read
# `skipif(not (INIT_SCRIPT.exists() and STOP_HOOK.exists()))`, which made the
# whole file abstain on the deletion of the very scripts it tests. It also made
# the per-test fix below decorative: a guard on one of two paths reads as
# protection while the other path still ships green. Both scripts are part of
# the product, so their absence is a failure and it surfaces at the first
# subprocess call with the path in the message.


# ---------------------------------------------------------------------------
# init-target-state.sh
# ---------------------------------------------------------------------------


def test_init_target_state_writes_a_state_file_for_a_node_input(tmp_path):
    """init-target-state.sh runs end to end for a node input and writes a state
    file, with no `fno` on PATH.

    Deliberately NOT the claim-acquisition test, though it used to claim to be.
    Its claim assertions sat behind `if "target_claim_key:" in text:` and that
    field is never written here, because nothing puts a resolvable `fno` on
    PATH. The body never executed, so the test proved nothing about acquiring a
    claim while its name said otherwise. The dead branch is gone and the name
    now matches what runs.

    Acquisition IS covered, properly, in test_target_node_claim.py: it installs
    a mock `fno` on PATH and asserts the key, the holder, and the TTL by value.
    """
    # Minimal fno-resolvable graph
    fno_home = tmp_path / ".fno-home"
    fno_home.mkdir()
    graph = fno_home / "graph.json"
    graph.write_text(
        '{"entries":[{"id":"ab-testit","plan_path":"plans/test.md",'
        '"status":"ready","priority":"p2","project":"fno"}]}'
    )

    # Set up a fake repo root with .fno/ and the resolvable plan path
    repo = tmp_path / "repo"
    (repo / "plans").mkdir(parents=True)
    (repo / "plans" / "test.md").write_text("# Test plan\n")
    (repo / ".fno").mkdir()
    # A REAL git repo on a feature branch. A hand-built `.git` holding only a
    # HEAD file makes `git rev-parse` fail, and the script's location gate then
    # refuses with rc=1 before it ever reaches the claim wiring this test is
    # about. That refusal was invisible while the assertion below was a skip.
    subprocess.run(["git", "init", "-q", "-b", "feature/sandbox"], cwd=repo, check=True)

    env = os.environ.copy()
    env.update({
        "TARGET_START": "1",
        "TARGET_INPUT": "ab-testit",
        "TARGET_SIZE": "S",
        "HOME": str(fno_home.parent),  # so the script's path-discovery works
    })

    # Run from the fake repo root
    # The full suite can be running other claim/process probes when this shell
    # starts; allow the real init path enough time to finish without masking a
    # genuine hang with a too-tight subprocess ceiling.
    result = subprocess.run(
        ["bash", str(INIT_SCRIPT)],
        cwd=repo,
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
    )

    # A missing state file is a FAILURE of the thing under test, never a reason
    # to abstain. This used to call pytest.skip with the script's own return
    # code embedded in the message: the code under test had failed and the
    # instrument reported green. An instrument that can no-op must not report
    # success on its no-op path.
    state = repo / ".fno" / "target-state.md"
    if not state.exists():
        pytest.fail(
            f"init-target-state.sh wrote no state file. That is the invariant "
            f"under test. rc={result.returncode}, "
            f"stderr={result.stderr[:500]!r}, stdout={result.stdout[:300]!r}"
        )

    text = state.read_text()
    # Assert what this sandbox actually produces, unconditionally. A `fno`-less
    # run still has to write a well-formed manifest naming its input.
    assert "fno_id:" in text, text
    assert 'input: "ab-testit"' in text, text
    assert result.returncode == 0, (
        f"rc={result.returncode}, stderr={result.stderr[:500]!r}"
    )


# ---------------------------------------------------------------------------
# release_graph_claim (via direct sourcing)
# ---------------------------------------------------------------------------


# test_stop_hook_contains_fno_claim_release_block removed (ab-d0337fbc): the
# stop hook is a read-only shim and no longer releases claims on exit; a dead
# session's claim goes stale via PID-liveness and is recovered by the next
# `fno agents claim acquire`. scripts/lib/claim-release.sh deleted with it.


def test_init_target_state_contains_fno_claim_acquire_block(tmp_path):
    """init-target-state.sh must contain the PR1 fno agents claim acquire block."""
    init_text = INIT_SCRIPT.read_text(encoding="utf-8")
    assert "fno agents claim acquire" in init_text, (
        "init-target-state.sh does not invoke `fno agents claim acquire`"
    )
    assert "target_claim_key" in init_text
    assert "target_claim_holder" in init_text

