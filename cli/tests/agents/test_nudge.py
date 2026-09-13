"""Tests for fno.agents.nudge — P2 loop-boundary inbox nudge (ab-098967b4).

Covers AC3-HP (one-line nudge surfaced), AC3-FR (surfaced once, durable copy
remains), AC3-EDGE (zero unread no-op; >1 unread surfaces oldest FIFO), and
AC4-UI (a reply is attributed as such). The bus is seeded directly.

The project broadcast leg is gone: fleet announcements ride their own
bus line and the `announce-cursors` store, never the nudge cursor.
"""
from __future__ import annotations

import pytest

from fno.agents import nudge
from fno.bus.log import Envelope, append
from fno.paths_testing import use_tmpdir


def _seed(to: str, from_: str, body: str, **kw) -> Envelope:
    env = Envelope.new(from_=from_, to=to, kind="send", body=body, **kw)
    append(env)
    return env


def _register_self(name: str, cwd: str) -> None:
    from fno.agents.registry import AgentEntry, write_registry
    write_registry([
        AgentEntry(
            name=name, harness="claude", cwd=cwd,
            log_path=f"/tmp/{name}.log", short_id=f"id-{name}", status="live",
        )
    ])


def test_ac3_hp_surfaces_one_line_nudge(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    _register_self("worker-b", "/x/proj")
    _seed("worker-b", "think-abc1", "does advance() resolve cwd per-key?", to_kind="name")
    line = nudge.peek_nudge("sess-1", "/x/proj")
    assert line is not None
    assert "think-abc1" in line
    assert "does advance" in line
    assert "fno agents mail unread" in line
    assert 'fno agents mail send think-abc1' in line
    assert "\n" not in line  # exactly one line


def test_ac3_fr_surfaces_once_durable_remains(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    _register_self("worker-b", "/x/proj")
    _seed("worker-b", "think-abc1", "a question", to_kind="name")
    first = nudge.peek_nudge("sess-1", "/x/proj")
    assert first is not None
    # Same unread persists -> NOT re-surfaced for the same session (cursor).
    second = nudge.peek_nudge("sess-1", "/x/proj")
    assert second is None
    # ...but the durable copy is still unread in the bus (never acked).
    from fno.bus.cursor import scan_unread

    assert len(scan_unread("worker-b", warn=False)) == 1


def test_ac3_edge_zero_unread_is_noop(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    assert nudge.peek_nudge("sess-1", "/x/proj") is None


def test_ac3_edge_fifo_oldest_first(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    _register_self("worker-b", "/x/proj")
    _seed("worker-b", "alice", "first question", to_kind="name")
    _seed("worker-b", "bob", "second question", to_kind="name")
    first = nudge.peek_nudge("sess-1", "/x/proj")
    second = nudge.peek_nudge("sess-1", "/x/proj")
    third = nudge.peek_nudge("sess-1", "/x/proj")
    assert "first question" in first  # oldest first (FIFO)
    assert "second question" in second
    assert third is None  # both surfaced, nothing left


def test_per_session_cursor_is_independent(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    _register_self("worker-b", "/x/proj")
    _seed("worker-b", "alice", "hello", to_kind="name")
    # A different session has its own cursor: it sees the same unread.
    assert nudge.peek_nudge("sess-1", "/x/proj") is not None
    assert nudge.peek_nudge("sess-2", "/x/proj") is not None


def test_ac4_ui_reply_attributed(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    _register_self("worker-b", "/x/proj")
    q = _seed("other", "me", "the question")
    _seed("worker-b", "recipient", "the answer", in_reply_to=q.id, to_kind="name")
    line = nudge.peek_nudge("sess-1", "/x/proj")
    assert line is not None
    assert "reply from recipient" in line


def test_no_registered_name_is_noop(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    # No live registry row at this cwd -> the nudge cannot know its own name,
    # and there is no project leg anymore. Addressed mail waits for a boundary
    # that knows the handle (the drain), never guesses.
    _seed("proj", "alice", "hello")
    assert nudge.peek_nudge("sess-1", "/x/proj") is None


# ---------------------------------------------------------------------------
# Group 1 (ab-ba91b807): by-name delivery + sender-exclusion at the boundary
# ---------------------------------------------------------------------------

def test_by_name_mail_is_surfaced(tmp_path, monkeypatch):
    # cv-d54ddd45: a message addressed to this worker by name must surface at
    # the loop boundary.
    use_tmpdir(monkeypatch, tmp_path)
    _register_self("worker-b", "/x/proj")
    _seed("worker-b", "alice", "rebase on main first", to_kind="name")
    line = nudge.peek_nudge("sess-1", "/x/proj")
    assert line is not None
    assert "rebase on main" in line
    assert "alice" in line


def test_by_name_mail_surfaced_once(tmp_path, monkeypatch):
    # The by-name cursor: the same message never surfaces twice to one session.
    use_tmpdir(monkeypatch, tmp_path)
    _register_self("worker-b", "/x/proj")
    _seed("worker-b", "alice", "one shot", to_kind="name")
    assert nudge.peek_nudge("sess-1", "/x/proj") is not None
    assert nudge.peek_nudge("sess-1", "/x/proj") is None


def test_announce_line_is_never_a_nudge(tmp_path, monkeypatch):
    # AC3-ISO: a fleet announcement rides to=fleet:<scope>; no session
    # handle ever equals it, so the nudge (and every addressed-mail reader)
    # cannot return it.
    use_tmpdir(monkeypatch, tmp_path)
    _register_self("worker-b", "/x/proj")
    _seed("fleet:all", "op", "one bus line", to_kind="fleet")
    assert nudge.peek_nudge("sess-1", "/x/proj") is None
    from fno.bus.cursor import scan_unread

    assert scan_unread("worker-b", warn=False) == []
