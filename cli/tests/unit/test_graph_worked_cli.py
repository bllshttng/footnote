"""Door tests: the hidden ``fno backlog worked`` authority verb.

The wheel spelling tombstones; these pin the native door's shapes through
the binary, with a PATH stub for the roster listing so the fleet read is
hermetic.
"""
from __future__ import annotations

import json
import os
import subprocess

import pytest

from tests.goldens._door import door, make_sandbox, seed_node, warm


def _claude_stub(root, rows):
    """A PATH dir whose `claude` answers one roster listing."""
    stub = root / "stubbin"
    stub.mkdir(exist_ok=True)
    body = json.dumps({"agents": rows})
    script = "#!/bin/sh\nprintf '%s' '" + body.replace("'", "'\\''") + "'\nexit 0\n"
    (stub / "claude").write_text(script)
    (stub / "claude").chmod(0o755)
    return str(stub)


def _entry(node_id, sessions):
    return seed_node(node_id, "ready", sessions=sessions)


def _open(phase, sid):
    return {
        "phase": phase,
        "harness": "claude",
        "session_id": sid,
        "started_at": "2026-09-09T00:00:00Z",
    }


def test_worked_json_names_worker(tmp_path):
    root = make_sandbox(tmp_path, [_entry("x-worked1", [_open("blueprint", "session-1")])])
    stub = _claude_stub(root, [{"sessionId": "session-1", "name": "bp-worker", "state": "working", "cwd": "/tmp"}])
    code, out, err = door(root, ["worked", "--json"], path_prepend=stub)
    assert code == 0, err
    rows = json.loads(out)
    assert rows == [{"id": "x-worked1", "status": "ready", "workers": ["bp-worker"], "phases": ["blueprint"]}]


def test_worked_text_is_one_line_per_node(tmp_path):
    root = make_sandbox(tmp_path, [_entry("x-worked2", [_open("do", "session-1")])])
    stub = _claude_stub(root, [{"sessionId": "session-1", "name": "do-worker", "state": "working", "cwd": "/tmp"}])
    code, out, err = door(root, ["worked"], path_prepend=stub)
    assert code == 0, err
    assert out == "x-worked2  ready  do-worker\n"


def test_worked_refuses_when_authority_unavailable(tmp_path):
    root = make_sandbox(tmp_path, [_entry("x-worked3", [_open("blueprint", "session-1")])])
    stub = _claude_stub(root, [42])
    code, out, err = door(root, ["worked", "--json"], path_prepend=stub)
    assert code == 1, out
    assert "worked authority unavailable" in err


def test_closed_session_phases_do_not_render(tmp_path):
    """phases names the work shapes live RIGHT NOW: a closed blueprint row
    beside a live do row must not read as active staffing."""
    root = make_sandbox(
        tmp_path,
        [
            _entry(
                "x-worked4",
                [
                    {**_open("blueprint", "session-1"), "ended_at": "2026-09-08T01:00:00Z"},
                    _open("do", "session-2"),
                ],
            )
        ],
    )
    stub = _claude_stub(
        root,
        [
            {"sessionId": "session-1", "name": "bp-worker", "state": "working", "cwd": "/tmp"},
            {"sessionId": "session-2", "name": "do-worker", "state": "working", "cwd": "/tmp"},
        ],
    )
    code, out, err = door(root, ["worked", "--json"], path_prepend=stub)
    assert code == 0, err
    # The store's read migrates the legacy `do` phase spelling; the door
    # answers the canonical vocabulary the store owns.
    assert json.loads(out)[0]["phases"] == ["execute"]
