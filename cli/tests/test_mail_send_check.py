"""``fno mail send --raw --check``: which verdict each lane deserves.

The point of ``--check`` is that a caller can gate ADVICE on it, so a wrong yes
is worse than no answer at all: the Stop hook that prescribes ``/compact`` reads
this verdict to decide between "fire it yourself" and "ask your operator".

The self/peer split on the mux lane is the subtle one and it is why this file
exists. ``_raw_send`` pastes with ``guarded=True``, which rides the server-side
turn-taken interlock and refuses ``EXIT_TARGET_NOT_IDLE`` while the recipient is
mid-turn. A session asking about ITSELF is mid-turn by construction -- it is
running the command inside its own turn -- so a live pane, not merely an exited
one, can never self-inject through that lane. Reporting a path there would be
exactly the false prescription the flag exists to prevent (codex review, PR
"context-nudge prescribed three things that cannot run").
"""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

REPO_CLI = Path(__file__).resolve().parents[1]


def _run(args: list[str], env_extra: dict[str, str], tmp_path: Path):
    """Run the CLI in a sandboxed state dir; return (stdout, exit code)."""
    state = tmp_path / ".fno"
    (state / "agents").mkdir(parents=True, exist_ok=True)
    settings = state / "settings.yaml"
    settings.write_text(f"schema_version: 1\nconfig:\n  state_dir: {state}/\n")
    (state / ".path-migration-done").touch()
    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(tmp_path),
        "FNO_CONFIG": str(settings),
        "PYTHONPATH": str(REPO_CLI / "src"),
        **env_extra,
    }
    proc = subprocess.run(
        [sys.executable, "-m", "fno.cli", "mail", "send", *args],
        capture_output=True, text=True, env=env, cwd=str(tmp_path), timeout=120,
    )
    return proc.stdout.strip(), proc.returncode


def _write_registry(tmp_path: Path, rows: list[dict]) -> None:
    (tmp_path / ".fno" / "agents").mkdir(parents=True, exist_ok=True)
    (tmp_path / ".fno" / "agents" / "registry.json").write_text(
        json.dumps({"schema_version": 13, "agents": rows})
    )


def _row(**over) -> dict:
    base = {
        "name": "worker", "harness": "claude", "cwd": "/tmp",
        "log_path": "/tmp/w", "status": "live", "short_id": "w",
        "harness_session_id": "sid-worker",
        "crown_level": None, "crown_scope": None, "crown_grantor": None,
    }
    base.update(over)
    return base


SELF_SID = "sid-me"
MUX = {"session": "fno", "pane_id": "%1"}


def test_self_on_guarded_mux_lane_is_not_injectable(tmp_path):
    """The finding this file was written for: a live pane is still no self path."""
    _write_registry(tmp_path, [_row(name="me", harness_session_id=SELF_SID, mux=MUX)])
    out, code = _run(
        ["/compact", "--to-self", "--raw", "--check"],
        {"CLAUDE_CODE_SESSION_ID": SELF_SID},
        tmp_path,
    )
    assert code == 1, out
    assert out.startswith("not-injectable:"), out
    assert "mid-turn" in out, "the reason must name WHY, or a reader retries forever"
    assert "operator" in out, "a no-path answer has to say what the session CAN do"


def test_peer_on_mux_lane_is_injectable(tmp_path):
    """A peer can be idle, so the same lane IS a path for a peer."""
    _write_registry(
        tmp_path,
        [_row(name="me", harness_session_id=SELF_SID),
         _row(name="peer", harness_session_id="sid-peer", mux=MUX)],
    )
    out, code = _run(
        ["peer", "/code-review", "--raw", "--check"],
        {"CLAUDE_CODE_SESSION_ID": SELF_SID},
        tmp_path,
    )
    assert code == 0, out
    assert out.startswith("injectable: mux-pane"), out


def test_no_registry_row_is_not_injectable(tmp_path):
    """The hand-started REPL: no row, so resolution fails before any transport."""
    _write_registry(tmp_path, [_row(name="someone-else")])
    out, code = _run(
        ["/compact", "--to-self", "--raw", "--check"],
        {"CLAUDE_CODE_SESSION_ID": SELF_SID},
        tmp_path,
    )
    assert code == 1, out
    assert out.startswith("not-injectable:"), out


def test_non_keystroke_lane_is_not_injectable(tmp_path):
    """A codex peer has no prompt line, so a slash payload could never fire."""
    _write_registry(
        tmp_path,
        [_row(name="me", harness_session_id=SELF_SID),
         _row(name="cx", harness="codex", harness_session_id="sid-cx")],
    )
    out, code = _run(
        ["cx", "/review", "--raw", "--check"],
        {"CLAUDE_CODE_SESSION_ID": SELF_SID},
        tmp_path,
    )
    assert code == 1, out
    assert "keystroke" in out, out


def test_check_without_raw_is_refused(tmp_path):
    """Only the raw lane has a keystroke path to have or lack."""
    _write_registry(tmp_path, [_row(name="me", harness_session_id=SELF_SID)])
    _out, code = _run(
        ["me", "hello", "--check"], {"CLAIUDE_UNUSED": "1"}, tmp_path
    )
    assert code == 2


@pytest.mark.parametrize("verdict_prefix", ["injectable:", "not-injectable:", "unmeasurable:"])
def test_every_verdict_is_a_distinct_prefix(verdict_prefix):
    """The three answers must stay prefix-distinguishable for shell callers.

    ``not-injectable:`` does NOT match a ``injectable:*`` glob (patterns anchor at
    the start), which is what lets the hook branch on the word rather than on an
    exit code a wrapper could swallow.
    """
    assert verdict_prefix.endswith(":")
    matches_yes = verdict_prefix.startswith("injectable:")
    assert matches_yes == (verdict_prefix == "injectable:")
