"""init-target-state.sh node:<id> claim acquire/refuse (ab-fcf9cec5).

Drives the real init script with a MOCK `fno` on PATH so the shell wiring is
covered deterministically (exit codes controlled, args/env recorded) without
depending on the installed `fno` snapshot. Proves:

  * a bare `/target ab-XXXX` input (no plan) acquires node:<id>,
  * the acquire uses a TTL and the global root (FNO_CLAIMS_ROOT=$HOME),
  * exit 1 (held-by-other) refuses via the .target-cancelled sentinel,
  * exit 2 (usage / stale-abi) does NOT block the session.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
INIT_SCRIPT = REPO_ROOT / "hooks" / "helpers" / "init-target-state.sh"

pytestmark = pytest.mark.skipif(
    not INIT_SCRIPT.exists(), reason="init-target-state.sh not present"
)

NODE_ID = "ab-deadbeef"  # matches ^ab-[0-9a-f]{8}$

MOCK_ABI = """#!/usr/bin/env bash
# Mock `fno`: log argv + the claims-root env, control claim-acquire exit code.
echo "ARGS:$* ROOT:${FNO_CLAIMS_ROOT:-UNSET}" >> "$MOCK_ABI_LOG"
if [[ "$1" == "claim" && "$2" == "acquire" ]]; then
  exit "${MOCK_ABI_ACQUIRE_RC:-0}"
fi
exit 0
"""


def _sandbox(tmp_path: Path):
    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    (home / ".fno" / "graph.json").write_text(
        '{"entries":[{"id":"%s","title":"t","status":"idea","priority":"p2",'
        '"project":"fno","plan_path":null}]}' % NODE_ID
    )
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    (repo / ".fno").mkdir()

    bindir = tmp_path / "bin"
    bindir.mkdir()
    mock = bindir / "fno"
    mock.write_text(MOCK_ABI)
    mock.chmod(0o755)
    log = tmp_path / "fno.log"

    # Pin the init script's `python3` to the interpreter running these tests.
    # The graph locked_by stamp shells out to scripts/roadmap-tasks.py, which
    # needs fno's dependencies importable; an ambient python3 (homebrew's, say)
    # has no typer, so the stamp degrades to its non-fatal warning and the
    # stamped-node assertion fails for a reason unrelated to the shell wiring
    # under test. Which python3 sits first on PATH must not decide that.
    py = bindir / "python3"
    py.write_text(f'#!/usr/bin/env bash\nexec "{sys.executable}" "$@"\n')
    py.chmod(0o755)

    env = os.environ.copy()
    env.update({
        "TARGET_START": "1",
        "TARGET_INPUT": NODE_ID,
        "TARGET_SIZE": "S",
        "HOME": str(home),
        "PATH": f"{bindir}:{env['PATH']}",
        "MOCK_ABI_LOG": str(log),
    })
    return repo, home, log, env


def _run_init(repo: Path, env: dict):
    return subprocess.run(
        ["bash", str(INIT_SCRIPT)],
        cwd=repo, env=env, capture_output=True, text=True, timeout=30,
    )


def _state(repo: Path) -> str:
    f = repo / ".fno" / "target-state.md"
    return f.read_text() if f.exists() else ""


def test_bare_node_id_acquires_global_ttl_claim(tmp_path):
    repo, home, log, env = _sandbox(tmp_path)
    env["MOCK_ABI_ACQUIRE_RC"] = "0"
    r = _run_init(repo, env)
    state = _state(repo)
    assert state, f"no state written: rc={r.returncode} stderr={r.stderr[:600]!r}"

    assert f'target_claim_key: "node:{NODE_ID}"' in state, state
    assert "target_claim_holder:" in state
    assert 'target_claim_ttl: "2h"' in state
    assert not (repo / ".fno" / ".target-cancelled").exists()

    log_text = log.read_text()
    acquire_lines = [ln for ln in log_text.splitlines()
                     if "claim acquire" in ln and NODE_ID in ln]
    assert acquire_lines, log_text
    line = acquire_lines[0]
    assert "--ttl 2h" in line, line
    assert f"ROOT:{home}" in line, "acquire must set FNO_CLAIMS_ROOT=$HOME: " + line


def test_codex_thread_identity_aligns_manifest_graph_and_claim(tmp_path):
    repo, home, log, env = _sandbox(tmp_path)
    (repo / "scripts").symlink_to(REPO_ROOT / "scripts", target_is_directory=True)
    thread_id = "019f48e4-codex-owner"
    env.update(
        {
            "CODEX_THREAD_ID": thread_id,
            "CODEX_SESSION_ID": "legacy-must-not-own",
            "MOCK_ABI_ACQUIRE_RC": "0",
        }
    )
    env.pop("CLAUDE_CODE_SESSION_ID", None)

    result = _run_init(repo, env)
    state = _state(repo)
    assert state, result.stderr
    graph = json.loads((home / ".fno" / "graph.json").read_text())["entries"][0]
    acquire = next(
        line for line in log.read_text().splitlines() if "claim acquire" in line
    )

    manifest_session_id = next(
        line.split(":", 1)[1].strip()
        for line in state.splitlines()
        if line.startswith("session_id:")
    )
    assert manifest_session_id != thread_id
    assert "-cx" in manifest_session_id
    assert f"codex_thread_id: {thread_id}" in state
    assert graph["session_id"] == thread_id
    assert f'--holder target-session:{thread_id}' in acquire
    assert f'target_claim_holder: "target-session:{thread_id}"' in state


def test_held_by_other_refuses(tmp_path):
    repo, home, log, env = _sandbox(tmp_path)
    env["MOCK_ABI_ACQUIRE_RC"] = "1"  # ClaimHeldByOther
    r = _run_init(repo, env)
    state = _state(repo)
    assert state, f"no state written: rc={r.returncode} stderr={r.stderr[:600]!r}"

    assert (repo / ".fno" / ".target-cancelled").exists(), \
        "exit 1 must touch the cancel sentinel"
    assert "target_claim_blocked_reason: claim_held_by_other" in state
    assert f'target_claim_key: "node:{NODE_ID}"' not in state


def test_non_contention_error_does_not_block(tmp_path):
    """exit 2 (e.g. an older fno rejecting --ttl) must not wedge the session."""
    repo, home, log, env = _sandbox(tmp_path)
    env["MOCK_ABI_ACQUIRE_RC"] = "2"
    r = _run_init(repo, env)
    state = _state(repo)
    assert state, f"no state written: rc={r.returncode} stderr={r.stderr[:600]!r}"

    assert not (repo / ".fno" / ".target-cancelled").exists(), \
        "a non-contention acquire failure must NOT block"
    assert "target_claim_blocked_reason: acquire_error_rc_2" in state
    assert f'target_claim_key: "node:{NODE_ID}"' not in state


SET_GATE = REPO_ROOT / "scripts" / "lib" / "set-gate.sh"


@pytest.mark.skipif(not SET_GATE.exists(), reason="set-gate.sh not present")
def test_set_gate_refresh_uses_ttl_and_global_root(tmp_path):
    """set-gate.sh refreshes the node claim with --ttl + the global root so a
    long phase cannot let a TTL claim shrink to MIN_TTL_MS and free the node."""
    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    # set-gate.sh validates the gate name against ./docs/architecture/
    # events-schema.yaml (relative to cwd) and refuses the flip if it's
    # missing - which would exit before the refresh hook. Stage a copy so the
    # flip succeeds and the refresh path actually runs.
    schema_src = REPO_ROOT / "docs" / "architecture" / "events-schema.yaml"
    if not schema_src.exists():
        pytest.skip("events-schema.yaml not present in this checkout")
    schema_dst = tmp_path / "docs" / "architecture" / "events-schema.yaml"
    schema_dst.parent.mkdir(parents=True)
    schema_dst.write_text(schema_src.read_text())
    state = tmp_path / "target-state.md"
    state.write_text(
        "---\n"
        "status: IN_PROGRESS\n"
        "session_id: test-sid\n"
        "provenance_nonce: deadbeef\n"
        'target_claim_key: "node:ab-deadbeef"\n'
        'target_claim_holder: "target-session:test-sid"\n'
        'target_claim_ttl: "2h"\n'
        "ledger_updated: false\n"
        "---\n"
    )
    bindir = tmp_path / "bin"
    bindir.mkdir()
    mock = bindir / "fno"
    mock.write_text(MOCK_ABI)
    mock.chmod(0o755)
    log = tmp_path / "fno.log"

    env = os.environ.copy()
    env.update({
        "HOME": str(home),
        "PATH": f"{bindir}:{env['PATH']}",
        "MOCK_ABI_LOG": str(log),
    })
    subprocess.run(
        ["bash", str(SET_GATE), str(state), "ledger_updated", "true", "register"],
        cwd=tmp_path, env=env, capture_output=True, text=True, timeout=20,
    )
    log_text = log.read_text() if log.exists() else ""
    refresh_lines = [ln for ln in log_text.splitlines() if "claim refresh" in ln]
    assert refresh_lines, f"refresh hook never invoked fno: {log_text!r}"
    line = refresh_lines[0]
    assert "--ttl 2h" in line, line
    assert f"ROOT:{home}" in line, "refresh must set FNO_CLAIMS_ROOT=$HOME: " + line


CLAIM_RELEASE = REPO_ROOT / "scripts" / "lib" / "claim-release.sh"


@pytest.mark.skipif(not CLAIM_RELEASE.exists(), reason="claim-release.sh not present")
def test_release_runs_when_graph_node_id_null(tmp_path):
    """release_graph_claim must release the fno node:<id> claim even when
    graph_node_id is null. The fno claim is acquired whenever target_claim_key
    is written (bare node-id idea node, or legacy-claim-refused), independent
    of graph_node_id; gating release on graph_node_id leaks the global lock for
    the full TTL and hides the node from selection (ab-fcf9cec5 regression)."""
    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    state = tmp_path / "target-state.md"
    state.write_text(
        "---\n"
        "status: IN_PROGRESS\n"
        "session_id: test-sid\n"
        "graph_node_id: null\n"
        'target_claim_key: "node:ab-deadbeef"\n'
        'target_claim_holder: "target-session:test-sid"\n'
        "---\n"
    )
    bindir = tmp_path / "bin"
    bindir.mkdir()
    mock = bindir / "fno"
    mock.write_text(MOCK_ABI)
    mock.chmod(0o755)
    log = tmp_path / "fno.log"

    driver = (
        f'log() {{ :; }}; SCRIPT_DIR="{REPO_ROOT}"; '
        f'source "{CLAIM_RELEASE}"; release_graph_claim "{state}"'
    )
    env = os.environ.copy()
    env.update({
        "HOME": str(home),
        "PATH": f"{bindir}:{env['PATH']}",
        "MOCK_ABI_LOG": str(log),
    })
    subprocess.run(["bash", "-c", driver], env=env,
                   capture_output=True, text=True, timeout=20)
    log_text = log.read_text() if log.exists() else ""
    release_lines = [ln for ln in log_text.splitlines() if "claim release" in ln]
    assert release_lines, (
        "fno claim release must run even when graph_node_id is null; got: "
        + repr(log_text)
    )
    line = release_lines[0]
    assert "node:ab-deadbeef" in line, line
    assert f"ROOT:{home}" in line, "release must set FNO_CLAIMS_ROOT=$HOME: " + line
