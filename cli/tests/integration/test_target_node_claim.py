"""init-target-state.sh node:<id> claim acquire/refuse (ab-fcf9cec5).

Drives the real init script with a MOCK `fno` on PATH so the shell wiring is
covered deterministically (exit codes controlled, args/env recorded) without
depending on the installed `fno` snapshot. Proves:

  * a bare `/target ab-XXXX` input (no plan) acquires node:<id>,
  * the acquire uses a TTL and the global root (FNO_CLAIMS_ROOT=$HOME),
  * exit 1 (held-by-other) refuses via the .target-cancelled sentinel,
  * exit 2 (usage / stale-fno) does NOT block the session.
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

# NO module-level skipif here, and the omission is load-bearing. It used to read
# `skipif(not INIT_SCRIPT.exists())`, so deleting the very script this file
# exists to test made the ENTIRE file pass silently. A missing script under test
# is a failure, not a reason to abstain, and it fails at the first subprocess
# call with the path in the message.

NODE_ID = "ab-deadbeef"  # matches ^ab-[0-9a-f]{8}$

MOCK_ABI = """#!/usr/bin/env bash
# Mock `fno`: log argv + the claims-root env, control claim-acquire exit code.
echo "ARGS:$* ROOT:${FNO_CLAIMS_ROOT:-UNSET}" >> "$MOCK_ABI_LOG"
if [[ "$1" == "agents" && "$2" == "claim" && "$3" == "acquire" ]]; then
  exit "${MOCK_ABI_ACQUIRE_RC:-0}"
fi
# `backlog get` is how the node guard establishes that a token IS a graph node.
# Delegating (rather than exiting 0 with no output, which reads as "not a node")
# exercises real resolution against the fixture graph.
if [[ "$1" == "backlog" && "$2" == "get" ]]; then
  exec python3 "$MOCK_ABI_SHIM" "${@:2}"
fi
# The graph lock stamp is the one call whose EFFECT a test asserts, so swallowing
# it as a bare success would hollow out the identity assertion. Delegate to the
# real writer under the pinned python3; the shim exposes graph.cli directly, so
# the leading `backlog` token is dropped.
if [[ "$1" == "backlog" && "$2" == "update" ]]; then
  # MOCK_ABI_STALE simulates an installed fno predating the harness flags.
  if [[ -n "${MOCK_ABI_STALE:-}" && "$*" == *--locked-by-harness* ]]; then
    echo "Error: No such option: --locked-by-harness" >&2
    exit 2
  fi
  exec python3 "$MOCK_ABI_SHIM" "${@:2}"
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

    # Pin `python3` to the interpreter running these tests. Production no longer
    # shells an interpreter for the stamp, but the mock's delegation to the
    # real writer below does, and it needs fno's dependencies importable; an
    # ambient python3 (homebrew's, say) has no typer. Which python3 sits first on
    # PATH must not decide whether the stamped-node assertion holds.
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
        "MOCK_ABI_SHIM": str(REPO_ROOT / "scripts" / "roadmap-tasks.py"),
    })
    return repo, home, log, env


def _run_init(repo: Path, env: dict):
    return subprocess.run(
        ["bash", str(INIT_SCRIPT)],
        cwd=repo,
        env=env,
        capture_output=True,
        text=True,
        # These integration paths exercise the real writer and can approach the
        # old ceiling under the parallel full suite. Bound hangs without turning
        # scheduler pressure into a false functional failure.
        timeout=90,
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


def test_stale_installed_fno_stamps_owner_only_and_says_so(tmp_path):
    """An fno predating the harness flags must still stamp the owner - but must
    NOT pass for a clean stamp, or the missing harness metadata goes silent."""
    repo, home, log, env = _sandbox(tmp_path)
    env["MOCK_ABI_ACQUIRE_RC"] = "0"
    env["MOCK_ABI_STALE"] = "1"

    r = _run_init(repo, env)
    graph = json.loads((home / ".fno" / "graph.json").read_text())["entries"][0]

    assert graph.get("locked_by"), f"owner must survive a stale fno: {graph}"
    assert not graph.get("locked_by_harness"), \
        "the stale fno rejected the harness flag; it must not appear stamped"
    assert "WITHOUT harness metadata" in r.stderr, \
        "degraded stamp must be announced, not silent: " + r.stderr[-600:]


def test_held_by_other_refuses(tmp_path):
    repo, home, log, env = _sandbox(tmp_path)
    env["MOCK_ABI_ACQUIRE_RC"] = "1"  # ClaimHeldByOther
    r = _run_init(repo, env)
    state = _state(repo)
    assert state, f"no state written: rc={r.returncode} stderr={r.stderr[:600]!r}"

    assert (repo / ".fno" / ".target-cancelled").exists(), \
        "exit 1 must touch the cancel sentinel"
    assert "target_claim_blocked_reason: claim_held_by_other" in state
    assert f"graph_node_id: {NODE_ID}" in state
    assert "graph_node_claim_refused: held_by_other" in state
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


# set-gate.sh and claim-release.sh were REMOVED from the product (the stop hook
# became a read-only shim). Their tests were `skipif(not <script>.exists())`, so
# each one could not fail on the deletion of the very script it existed to test,
# and both have been skipping silently ever since. A test for functionality that
# no longer ships is dead code, so it is deleted here rather than converted to a
# hard failure that would keep CI permanently red for a deliberate removal.
