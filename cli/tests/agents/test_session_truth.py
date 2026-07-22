"""Unit tests for fno.agents.session_truth (x-a472 deliverable B).

The truth verb classifies a worker's supervision state from its transcript TAIL
only -- never from pid/argv/daemon/state.json state (all caught lying about a
live session in one evening). Tests exercise the pure classifier and the
resolve+read path (through the x-a472-fixed transcript resolver, so a worktree
transcript is found when the caller passes the canonical cwd).
"""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest


# ---------------------------------------------------------------------------
# classify_tail: pure signal precedence (AC2-HP / AC2-EDGE)
# ---------------------------------------------------------------------------

@pytest.mark.parametrize(
    "role,text,age,expected",
    [
        ("assistant", "<promise>MISSION COMPLETE: shipped</promise>", 10, "done"),
        ("assistant", '<watching reason="ci" pr="5" timeout="30m">', 10, "watching"),
        ("assistant", "Which base branch should I use?", 10, "your-move"),
        ("assistant", '<help reason="stuck" evidence="x">need a decision</help>', 10, "your-move"),
        ("assistant", "still grinding on the parser", 10, "working"),
        ("assistant", "still grinding on the parser", 99999, "stalled"),
        ("assistant", "still grinding on the parser", None, "working"),  # can't prove stalled
        # a content signal beats the mtime fallback even when old
        ("assistant", "<promise>done</promise>", 99999, "done"),
        ("assistant", "anything ending in a question?", 99999, "your-move"),
        # watching outranks promise when both appear (runtime parks watching)
        ("assistant", "<promise>done</promise> but also <watching pr=1>", 10, "watching"),
        # exact-marker discipline: a lookalike word is NOT a promise
        ("assistant", "we <promised> to fix it, still working", 10, "working"),
        # trailing USER turn clears a stale assistant signal -> worker's move
        ("user", "<promise>done</promise> (quoted by the operator)", 10, "working"),
        ("user", "here is your answer", 99999, "stalled"),
    ],
)
def test_classify_tail_precedence(role, text, age, expected):
    from fno.agents.session_truth import classify_tail

    assert classify_tail(role, text, age) == expected


def test_classify_tail_empty_text_fresh_is_working():
    from fno.agents.session_truth import classify_tail

    assert classify_tail("assistant", "", 5) == "working"
    assert classify_tail("assistant", None, 5) == "working"


# ---------------------------------------------------------------------------
# resolve_session_truth: resolve + read the tail (AC2-HP / AC2-ERR)
# ---------------------------------------------------------------------------

def _write_claude_transcript(
    projects_root: Path, cwd: str, sid: str, turns: list, *, dir_slug: str = ""
) -> Path:
    """Write a transcript. ``turns`` items are either a str (assistant text) or a
    (role, text) tuple, so a test can end on a user turn."""
    slug = dir_slug or cwd.replace("/", "-").replace(".", "-")
    d = projects_root / slug
    d.mkdir(parents=True, exist_ok=True)
    path = d / f"{sid}.jsonl"
    lines = []
    for turn in turns:
        role, text = ("assistant", turn) if isinstance(turn, str) else turn
        lines.append(
            json.dumps(
                {
                    "type": role,
                    "message": {"role": role, "content": [{"type": "text", "text": text}]},
                }
            )
        )
    path.write_text("\n".join(lines) + "\n")
    return path


def _resolver(session):
    def r(_handle):
        return session, []
    return r


def test_resolve_reads_worktree_transcript_your_move(tmp_path):
    """AC2-HP + integrates the resolver fix: session dispatched with canonical
    cwd, live transcript in the worktree dir, last turn ends in a question."""
    from fno.agents.session_truth import resolve_session_truth

    canonical = "/Users/bb16/code/footnote/footnote"
    worktree = "/Users/bb16/code/footnote/footnote/.claude/worktrees/x-a472"
    sid = "4ec8a08b-9fe7-4550-8e40-00c7fd4e600a"
    _write_claude_transcript(tmp_path, worktree, sid, ["Should I rebase onto main?"])

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=canonical, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path, now_s=2_000_000_000.0
    )

    assert result["state"] == "your-move"
    assert result["session_id"] == sid


def test_resolve_done_from_promise(tmp_path):
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "abcdef12-1234-5678-9abc-def012345678"
    _write_claude_transcript(tmp_path, cwd, sid, ["<promise>MISSION COMPLETE: x</promise>"])

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth("w1", resolve=_resolver(session), projects_root=tmp_path)
    assert result["state"] == "done"


def test_resolve_user_turn_after_promise_is_working(tmp_path):
    """P2 fix: assistant emits <promise>, then the operator sends a new task.
    The last turn is the user's, so the worker owes the next move -> working,
    not a stale done."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "abcdef12-9999-0000-0000-000000000000"
    _write_claude_transcript(
        tmp_path,
        cwd,
        sid,
        ["<promise>MISSION COMPLETE</promise>", ("user", "actually, also handle the edge case")],
    )

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path, now_s=None
    )
    assert result["state"] == "working"


def test_resolve_stalled_when_transcript_old(tmp_path):
    import os

    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "abcdef12-0000-0000-0000-000000000000"
    path = _write_claude_transcript(tmp_path, cwd, sid, ["grinding on the parser"])
    os.utime(path, (1000, 1000))  # ancient

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path, now_s=2_000_000_000.0
    )
    assert result["state"] == "stalled"


def test_resolve_unknown_when_handle_unresolved(tmp_path):
    """AC2-ERR: unresolvable handle -> unknown/not-found, never raises."""
    from fno.agents.session_truth import resolve_session_truth

    def miss(_handle):
        return None, ["w1", "w2"]

    result = resolve_session_truth("nope", resolve=miss, projects_root=tmp_path)
    assert result["state"] == "unknown"
    assert result["reason"] == "not-found"
    assert result["suggestions"] == ["w1", "w2"]


def test_resolve_unknown_when_no_records(tmp_path):
    """AC2-ERR: session resolves but transcript has no renderable records."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "beadface-0000-0000-0000-000000000000"
    # A dir + empty transcript file: resolves to a path but zero records.
    _write_claude_transcript(tmp_path, cwd, sid, [])

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth("w1", resolve=_resolver(session), projects_root=tmp_path)
    assert result["state"] == "unknown"
    assert result["reason"] == "no-records"


def test_resolve_never_raises_on_broken_resolver(tmp_path):
    from fno.agents.session_truth import resolve_session_truth

    def boom(_handle):
        raise RuntimeError("resolver blew up")

    result = resolve_session_truth("w1", resolve=boom, projects_root=tmp_path)
    assert result["state"] == "unknown"


# ---------------------------------------------------------------------------
# render_truth: one legible human line (AC3-UI)
# ---------------------------------------------------------------------------

def test_render_states():
    from fno.agents.session_truth import render_truth

    line = render_truth(
        {"handle": "w1", "state": "your-move", "reason": None,
         "last_activity_age_s": 240, "session_id": "s", "suggestions": []}
    )
    assert line.startswith("truth w1: your-move")
    assert "4m" in line

    unk = render_truth(
        {"handle": "nope", "state": "unknown", "reason": "not-found",
         "last_activity_age_s": None, "session_id": None, "suggestions": ["w1"]}
    )
    assert "unknown" in unk and "not-found" in unk and "w1" in unk
