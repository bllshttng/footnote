"""Tests for ``fno agents peek`` (x-05da) — the read-only twin of mail send.

Grouped by the plan's verify slices:
  peek_resolve       — US1: resolver parity + exit-13 miss (AC1-HP, AC1-ERR)
  peek_reader        — US2: recent_records claude/codex/unsupported (AC2-*)
  peek_tail_follow   — US3: --lines / --json / no-activity / follow (AC1-EDGE/FR/UI)
  peek_status_stream — US4: dual-envelope status fast-path (AC3-EDGE)
"""
from __future__ import annotations

import io
import json
from pathlib import Path

from fno.agents.peek import (
    ObserveUnsupported,
    Record,
    peek,
    recent_records,
)


class _Session:
    """Minimal DiscoveredSession stand-in (peek reads attrs by getattr)."""

    def __init__(self, agent="claude", session_id="sid-123", short_id="abc12345", cwd="/tmp/proj"):
        self.agent = agent
        self.session_id = session_id
        self.short_id = short_id
        self.cwd = cwd


def _claude_transcript(root: Path, cwd: str, session_id: str, lines: list[str]) -> Path:
    slug = cwd.replace("/", "-").replace(".", "-")
    d = root / slug
    d.mkdir(parents=True, exist_ok=True)
    p = d / f"{session_id}.jsonl"
    p.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return p


def _user(text):
    return json.dumps({"type": "user", "message": {"role": "user", "content": text}})


def _assistant(text):
    return json.dumps(
        {"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}}
    )


# --------------------------------------------------------------------------
# peek_resolve — US1
# --------------------------------------------------------------------------


def test_peek_resolve_hit_prints_header_and_records(tmp_path):
    """AC1-HP: a resolvable peer shows a header + its recent records."""
    sess = _Session()
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, [_user("hello"), _assistant("hi there")])
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "worker-x",
        lines=5,
        stdout=out,
        stderr=err,
        resolve=lambda h: (sess, []),
        projects_root=tmp_path,
    )
    assert rc == 0
    body = out.getvalue()
    assert "peer worker-x: agent=claude short_id=abc12345" in body
    assert "user: hello" in body
    assert "assistant: hi there" in body


def test_peek_resolve_miss_exits_13_with_suggestions(tmp_path):
    """AC1-ERR: an unresolvable handle exits 13, parity with mail send."""
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "nope",
        stdout=out,
        stderr=err,
        resolve=lambda h: (None, ["worker-a", "worker-b"]),
        projects_root=tmp_path,
    )
    assert rc == 13
    assert "peer not found: nope" in err.getvalue()
    assert "worker-a" in err.getvalue()


# --------------------------------------------------------------------------
# peek_reader — US2
# --------------------------------------------------------------------------


def test_peek_reader_claude_tails_and_skips_torn(tmp_path):
    """AC2-HP + AC2-EDGE: claude arm parses records, skips a torn trailing line."""
    sess = _Session()
    lines = [_user("first"), _assistant("second"), '{"type":"assistant","message":{"role":"assist']
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, lines)
    recs = recent_records("claude", sess.session_id, sess.cwd, 10, projects_root=tmp_path)
    assert [r.text for r in recs] == ["first", "second"]


def test_peek_reader_unsupported_raises(tmp_path):
    """AC2-ERR: an agent with no arm raises ObserveUnsupported."""
    try:
        recent_records("gemini", "sid", "/tmp/x", 5)
    except ObserveUnsupported as exc:
        assert exc.agent == "gemini"
    else:
        raise AssertionError("expected ObserveUnsupported")


def test_peek_unsupported_agent_exit_1(tmp_path):
    """AC2-ERR at the command layer: legible message, exit 1 (not 13)."""
    sess = _Session(agent="gemini")
    out, err = io.StringIO(), io.StringIO()
    rc = peek("g", stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path)
    assert rc == 1
    assert "observe not yet supported for gemini" in err.getvalue()


def test_peek_reader_codex(tmp_path, monkeypatch):
    """AC2-HP: codex arm locates the rollout by session_meta id and parses it."""
    sess = _Session(agent="codex", session_id="cx-999")
    day = tmp_path / "2025" / "10" / "16"
    day.mkdir(parents=True)
    rollout = day / "rollout-2025-10-16T00-00-00-cx-999.jsonl"
    rollout.write_text(
        "\n".join(
            [
                json.dumps({"type": "session_meta", "payload": {"id": "cx-999", "cwd": "/tmp/proj"}}),
                json.dumps(
                    {
                        "type": "response_item",
                        "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "ping"}]},
                    }
                ),
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    recs = recent_records("codex", "cx-999", "/tmp/proj", 10, codex_sessions_dir=tmp_path)
    assert [(r.role, r.text) for r in recs] == [("user", "ping")]


# --------------------------------------------------------------------------
# peek_tail_follow — US3
# --------------------------------------------------------------------------


def test_peek_lines_zero_header_only(tmp_path):
    """Boundary: --lines 0 prints the header and zero records, exit 0."""
    sess = _Session()
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, [_user("hello")])
    out, err = io.StringIO(), io.StringIO()
    rc = peek("w", lines=0, stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path)
    assert rc == 0
    assert "user: hello" not in out.getvalue()
    assert "peer w:" in out.getvalue()


def test_peek_no_activity_yet(tmp_path):
    """AC1-EDGE: resolved but no transcript file → 'no activity yet', exit 0."""
    sess = _Session()  # no transcript written
    out, err = io.StringIO(), io.StringIO()
    rc = peek("w", stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path)
    assert rc == 0
    assert "no activity yet" in out.getvalue()


def test_peek_json_emits_rows(tmp_path):
    """--json emits one JSON object per record (AC1-UI, machine-readable)."""
    sess = _Session()
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, [_assistant("done")])
    out, err = io.StringIO(), io.StringIO()
    rc = peek("w", json_out=True, stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path)
    assert rc == 0
    rows = [json.loads(l) for l in out.getvalue().splitlines() if l.strip()]
    assert {"role": "assistant", "text": "done"} in rows


def test_peek_follow_exits_when_peer_not_live(tmp_path):
    """AC1-FR: --follow exits (no spin) once is_live() reports the peer gone."""
    sess = _Session()
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, [_user("hi")])
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "w",
        follow=True,
        stdout=out,
        stderr=err,
        resolve=lambda h: (sess, []),
        projects_root=tmp_path,
        is_live=lambda: False,
    )
    assert rc == 0
    assert "peer ended" in err.getvalue()


# --------------------------------------------------------------------------
# peek_status_stream — US4
# --------------------------------------------------------------------------


def test_peek_status_stream_dual_envelope(tmp_path):
    """AC3-EDGE: both {type,data} and {kind,..flat} status events render; a
    neither-shape record is skipped (fast-path stays present, not partial)."""
    sess = _Session(short_id="abc12345", session_id="sid-123")
    events = tmp_path / "events.jsonl"
    events.write_text(
        "\n".join(
            [
                json.dumps({"type": "task_started", "data": {"short_id": "abc12345"}}),
                json.dumps({"kind": "task_done", "short_id": "abc12345"}),
                json.dumps({"type": "unrelated_event", "data": {"short_id": "abc12345"}}),
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    out, err = io.StringIO(), io.StringIO()
    rc = peek(
        "w",
        stdout=out,
        stderr=err,
        resolve=lambda h: (sess, []),
        projects_root=tmp_path,
        events_path=events,
    )
    assert rc == 0
    body = out.getvalue()
    assert "[task_started]" in body
    assert "[task_done]" in body
    assert "unrelated_event" not in body


def test_peek_status_absent_falls_through_to_transcript(tmp_path):
    """US4: no status events → fall through to the transcript, no error."""
    sess = _Session()
    _claude_transcript(tmp_path, sess.cwd, sess.session_id, [_user("via transcript")])
    events = tmp_path / "events.jsonl"
    events.write_text(json.dumps({"type": "some_other_event"}) + "\n", encoding="utf-8")
    out, err = io.StringIO(), io.StringIO()
    rc = peek("w", stdout=out, stderr=err, resolve=lambda h: (sess, []), projects_root=tmp_path, events_path=events)
    assert rc == 0
    assert "user: via transcript" in out.getvalue()
