"""``fno mail send --raw --check``: which verdict each lane deserves.

The point of ``--check`` is that a caller can gate ADVICE on it, so a wrong yes
is worse than no answer at all: the Stop hook that prescribes ``/compact`` reads
this verdict to decide between "fire it yourself" and "ask your operator".

The mux lane used to carry a self/peer split, and this file was written to pin
it: ``_raw_send`` pasted with ``guarded=True``, which rode the server-side
turn-taken interlock and refused ``EXIT_TARGET_NOT_IDLE`` while the recipient
was mid-turn. A session asking about ITSELF is mid-turn by construction -- it
is running the command inside its own turn -- so a live pane, not merely an
exited one, could never self-inject through that lane, and reporting a path
would have been exactly the false prescription the flag exists to prevent
(codex review, PR "context-nudge prescribed three things that cannot run").

Node x-1904 removed that veto. Measurement (not inference) showed the guard
was ``rerun_allowed``, borrowed from the rerun verb, and a busy claude session
actually enqueues an injected paste rather than corrupting its composer -- see
the doc comment on ``rerun_allowed`` in ``crates/fno/src/server.rs`` for the
specimen. ``_raw_send`` now pastes unguarded and confirms by content against
the recipient's own transcript, landing even mid-turn, the same property the
control.sock lane already had (which is why control.sock never carried this
split either). So self and peer are no longer a structurally different
question on the mux lane: both get "the row recording a pane IS the path."
"""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

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


def test_self_on_unguarded_mux_lane_is_injectable(tmp_path):
    """x-1904: the de-veto makes a self-directed mux pane a path again.

    Before the de-veto, a live pane was structurally unreachable for a self
    send (the guard refused any mid-turn recipient, and self is always
    mid-turn). Now the paste is unguarded and confirms by content, landing
    even mid-turn -- the same property the control.sock lane already had."""
    _write_registry(tmp_path, [_row(name="me", harness_session_id=SELF_SID, mux=MUX)])
    out, code = _run(
        ["/compact", "--to-self", "--raw", "--check"],
        {"CLAUDE_CODE_SESSION_ID": SELF_SID},
        tmp_path,
    )
    assert code == 0, out
    assert out.startswith("injectable: mux-pane"), out


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


def test_malformed_payload_is_a_usage_error_not_a_verdict(tmp_path):
    """A bad payload says nothing about the session, so it must not print a verdict.

    Reporting ``not-injectable`` here would assert exactly the kind of unestablished
    claim ``--check`` exists to prevent: a caller gating advice would tell a session
    with a perfectly good path to go ask its operator.
    """
    _write_registry(tmp_path, [_row(name="me", harness_session_id=SELF_SID)])
    out, code = _run(
        ["me", "not-a-slash-verb", "--raw", "--check"],
        {"CLAUDE_CODE_SESSION_ID": SELF_SID},
        tmp_path,
    )
    assert code == 2, out
    assert "not-injectable" not in out, out


def test_unreadable_registry_is_unmeasurable_not_a_no_path(tmp_path):
    """"I could not read the evidence" is the third answer, never a measured no.

    The fixture used to be a future schema_version. That is no longer unreadable:
    a newer store now reads forward, precisely so one source-ahead writer cannot
    brick every reader on the machine. A torn file is what stays unreadable by
    design, so it is what this property is measured against now.
    """
    (tmp_path / ".fno" / "agents").mkdir(parents=True, exist_ok=True)
    (tmp_path / ".fno" / "agents" / "registry.json").write_text("{ not json")
    out, code = _run(
        ["/compact", "--to-self", "--raw", "--check"],
        {"CLAUDE_CODE_SESSION_ID": SELF_SID},
        tmp_path,
    )
    assert code == 3, out
    assert out.startswith("unmeasurable:"), out
