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


def _write_claude_transcript_with_model(
    projects_root: Path, cwd: str, sid: str, model: str, turns: int = 3
) -> Path:
    """A transcript whose assistant messages carry ``model``, as claude writes it."""
    slug = cwd.replace("/", "-").replace(".", "-")
    d = projects_root / slug
    d.mkdir(parents=True, exist_ok=True)
    path = d / f"{sid}.jsonl"
    lines = []
    for i in range(turns):
        lines.append(json.dumps({"type": "user", "message": {
            "role": "user", "content": [{"type": "text", "text": f"go {i}"}]}}))
        lines.append(json.dumps({"type": "assistant", "message": {
            "role": "assistant", "model": model,
            "content": [{"type": "text", "text": f"working on it {i}"}]}}))
    path.write_text("\n".join(lines) + "\n")
    return path


def _write_codex_rollout(
    sessions_dir: Path, cwd: str, sid: str, model: str
) -> Path:
    """A codex rollout: session_meta for identity, turn_context for the model."""
    d = sessions_dir / "2026" / "08" / "04"
    d.mkdir(parents=True, exist_ok=True)
    path = d / f"rollout-2026-08-04T10-00-00-{sid}.jsonl"
    lines = [
        json.dumps({"type": "session_meta", "payload": {"id": sid, "cwd": cwd}}),
        json.dumps({"type": "turn_context", "payload": {"model": model}}),
        json.dumps({"type": "response_item", "payload": {
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "on it"}]}}),
    ]
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


def test_default_resolver_can_read_stalled_session(monkeypatch):
    """Truth itself must resolve rows that live-routing correctly excludes."""
    from fno.agents import discover, session_truth

    seen = {}

    def fake(handle, **kwargs):
        seen.update(kwargs)
        return None, []

    monkeypatch.setattr(discover, "resolve_or_suggest", fake)
    session_truth._default_resolve("deadbeef")
    assert seen["require_alive"] is False


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
# opencode activity age from message timestamps (codex/bot P2)
# ---------------------------------------------------------------------------

def test_opencode_activity_epoch_from_messages(tmp_path):
    import sqlite3

    from fno.agents.session_truth import _opencode_activity_epoch

    db = tmp_path / "opencode.db"
    con = sqlite3.connect(db)
    con.execute(
        "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, "
        "time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL)"
    )
    # time_updated is epoch MILLISECONDS; newest wins.
    con.executemany(
        "INSERT INTO message VALUES (?,?,?,?,?)",
        [
            ("m1", "ses_abc", 1_000_000, 1_000_000_000_000, "{}"),
            ("m2", "ses_abc", 2_000_000, 1_784_000_000_000, "{}"),
            ("m3", "ses_other", 3_000_000, 1_999_999_999_999, "{}"),
        ],
    )
    con.commit()
    con.close()

    epoch = _opencode_activity_epoch("ses_abc", db)
    assert epoch == 1_784_000_000.0  # newest for ses_abc, scaled ms -> s

    assert _opencode_activity_epoch("ses_missing", db) is None


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


def test_resolver_crash_is_distinct_from_a_routine_miss(tmp_path):
    """A crashing resolver must not share not-found's reason.

    Callers suppress the routine not-found (it is the expected answer for a
    reaped or non-claude handle); sharing a reason would silence a malfunction
    under the same label. Both still exit 13 - only the reason differs.
    """
    from fno.agents.session_truth import resolve_session_truth

    def boom(_handle):
        raise RuntimeError("registry unreadable")

    result = resolve_session_truth("nope", resolve=boom, projects_root=tmp_path)
    assert result["state"] == "unknown"
    assert result["reason"] == "resolver-error"
    assert result["reason"] != "not-found"


# ---------------------------------------------------------------------------
# observed_model: the route a worker is ACTUALLY on, read from its transcript
#
# The whole point is that this is DERIVED, never recorded. A route stamped at
# spawn reports intent, so it would have printed the intended model in exactly
# the scenario that motivated this reading (an operator who suspects a silent
# fallback). Every test below feeds a transcript, never a spawn argument.
# ---------------------------------------------------------------------------

def test_observed_model_from_claude_transcript(tmp_path):
    """AC1-HP: a worker routed to zai whose transcript records glm-5.2."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-0000-0000-0000-000000000001"
    _write_claude_transcript_with_model(tmp_path, cwd, sid, "glm-5.2", turns=3)

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )

    assert result["observed_model"] == {
        "kind": "observed",
        "model": "glm-5.2",
        "samples": 3,
    }


def test_observed_model_no_transcript_still_renders(tmp_path):
    """AC3-ERR: no transcript file -> no-transcript, the row still resolves."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-0000-0000-0000-000000000002"

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )

    assert result["observed_model"] == {"kind": "no-transcript"}
    # A missing transcript is not a fault: truth still answers, no exception.
    assert result["state"] == "unknown"


def test_observed_model_no_model_yet_is_separable_from_no_transcript(tmp_path):
    """AC4-ERR: a transcript that exists but carries no answered turn.

    This is the diagnostic the whole four-variant shape exists for: a worker
    that came up and never processed a turn must not read like one that was
    spawned two seconds ago.
    """
    from fno.agents.session_truth import observed_model, resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-0000-0000-0000-000000000003"
    # User turns only: the session exists, nothing has answered.
    path = _write_claude_transcript(tmp_path, cwd, sid, [("user", "start the work")])

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )

    assert result["observed_model"] == {"kind": "no-model-yet"}
    assert observed_model("claude", path) != {"kind": "no-transcript"}


def test_observed_model_skips_claudes_own_synthetic_records(tmp_path):
    """A `<synthetic>` record is claude's own notice, not a vendor answer.

    It is written as `type: assistant` for an API error or an interrupt and so
    lands LAST exactly when an operator is checking the route (measured
    2026-08-04: 893 occurrences locally, and 14 of 200 transcripts end on one). Reporting it as the model would answer the
    fallback question with a string no vendor ever served.
    """
    from fno.agents.session_truth import observed_model

    path = tmp_path / "errored.jsonl"
    path.write_text(
        json.dumps({"type": "assistant", "message": {"model": "glm-5.2"}})
        + "\n"
        + json.dumps({"type": "assistant", "message": {"model": "<synthetic>"}})
        + "\n"
    )

    assert observed_model("claude", path) == {
        "kind": "observed",
        "model": "glm-5.2",
        "samples": 1,
    }

    # Nothing but synthetic notices is "never answered", not an observation.
    path.write_text(
        json.dumps({"type": "assistant", "message": {"model": "<synthetic>"}}) + "\n"
    )
    assert observed_model("claude", path) == {"kind": "no-model-yet"}


def test_observed_model_torn_final_line_is_unreadable_then_succeeds(tmp_path):
    """AC5-CON: read while the writer is mid-line; no lock, correct next read."""
    from fno.agents.session_truth import observed_model

    path = tmp_path / "live.jsonl"
    complete = json.dumps({"type": "assistant", "message": {"model": "glm-5.2"}})
    # The writer got half of the next record out.
    path.write_text(complete + "\n" + '{"type":"assist')

    torn = observed_model("claude", path)
    assert torn["kind"] == "unreadable"
    assert "torn" in torn["reason"]

    # The writer finishes the line; the very next read succeeds.
    path.write_text(complete + "\n" + complete + "\n")
    assert observed_model("claude", path) == {
        "kind": "observed",
        "model": "glm-5.2",
        "samples": 2,
    }


def test_observed_model_unreadable_reason_when_path_is_not_a_file(tmp_path):
    """A path that exists but cannot be read is unreadable, never no-transcript:
    invisibility and absence must stay distinguishable."""
    from fno.agents.session_truth import observed_model

    d = tmp_path / "a-directory.jsonl"
    d.mkdir()
    result = observed_model("claude", d)
    assert result["kind"] == "unreadable"
    assert result["reason"]


def test_observed_model_from_codex_rollout(tmp_path):
    """AC6-HP: codex records the model on turn_context; same resolver, same key."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "019fcde6-b8c0-7000-8000-000000000004"
    _write_codex_rollout(tmp_path, cwd, sid, "gpt-5.6-sol")

    session = SimpleNamespace(agent="codex", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1",
        resolve=_resolver(session),
        codex_sessions_dir=tmp_path,
    )

    assert result["observed_model"] == {
        "kind": "observed",
        "model": "gpt-5.6-sol",
        "samples": 1,
    }


def test_observed_model_reports_a_silent_fallback_verbatim(tmp_path):
    """AC7-ERR: a worker INTENDED for zai that is answering as claude-*.

    Reported verbatim, which is what makes intent and outcome visibly disagree.
    A recorded-at-spawn field would have printed `glm-5.2` here with full
    confidence -- that is the defect this reading exists to avoid.
    """
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-0000-0000-0000-000000000005"
    _write_claude_transcript_with_model(tmp_path, cwd, sid, "claude-opus-5", turns=2)

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )

    assert result["observed_model"]["model"] == "claude-opus-5"


def test_observed_model_reads_a_bounded_tail_not_the_whole_file(tmp_path):
    """A multi-MB transcript costs the same fixed read as a fresh one, and the
    window boundary never yields a torn-line verdict of its own."""
    from fno.agents.session_truth import _MODEL_TAIL_BYTES, observed_model

    path = tmp_path / "big.jsonl"
    filler = json.dumps({"type": "user", "message": {"role": "user",
                                                     "content": "x" * 500}})
    recent = json.dumps({"type": "assistant", "message": {"model": "glm-5.2"}})
    with path.open("w") as fh:
        for _ in range(_MODEL_TAIL_BYTES // len(filler) + 50):
            fh.write(filler + "\n")
        fh.write(recent + "\n")

    assert path.stat().st_size > _MODEL_TAIL_BYTES
    assert observed_model("claude", path) == {
        "kind": "observed",
        "model": "glm-5.2",
        "samples": 1,
    }


def test_observed_model_separates_never_from_not_yet(tmp_path):
    """A harness with no per-session file is not the same as one whose file has
    not appeared yet.

    opencode keeps a shared SQLite store, so it would report "no transcript yet"
    forever -- which the contract documents as "spawned two seconds ago". A
    permanent absence and a pending one are different facts, and this shape
    exists precisely so they do not collapse.
    """
    from fno.agents.session_truth import observed_model

    assert observed_model("opencode", tmp_path / "anything") == {
        "kind": "not-file-backed"
    }
    # claude IS file-backed, so a missing file there really is "not yet".
    assert observed_model("claude", tmp_path / "missing.jsonl") == {
        "kind": "no-transcript"
    }


# ---------------------------------------------------------------------------
# render_truth: the model on the line (US1 + US3)
# ---------------------------------------------------------------------------

def _truth_result(observed):
    return {
        "handle": "569d8d39",
        "state": "working",
        "reason": None,
        "last_activity_age_s": 6,
        "session_id": "s",
        "observed_model": observed,
        "suggestions": [],
    }


def test_render_truth_names_the_observed_model():
    """AC1-HP: the operator asks one verb and gets the real model."""
    from fno.agents.session_truth import render_truth

    line = render_truth(
        _truth_result({"kind": "observed", "model": "glm-5.2", "samples": 300})
    )
    assert line == "truth 569d8d39: working on glm-5.2 (active, last activity 6s ago)"


def test_render_truth_no_model_yet_does_not_read_as_healthy():
    """AC4-ERR: distinct from no-transcript, and visible as its own state."""
    from fno.agents.session_truth import render_truth

    line = render_truth(_truth_result({"kind": "no-model-yet"}))
    assert "no model yet" in line
    assert render_truth(_truth_result({"kind": "no-transcript"})) != line


def test_render_truth_omits_the_clause_when_there_is_no_transcript():
    """A worker spawned two seconds ago must not look broken."""
    from fno.agents.session_truth import render_truth

    line = render_truth(_truth_result({"kind": "no-transcript"}))
    assert line == "truth 569d8d39: working (active, last activity 6s ago)"


def test_render_truth_says_so_when_the_transcript_is_unreadable():
    from fno.agents.session_truth import render_truth

    line = render_truth(_truth_result({"kind": "unreadable", "reason": "EIO"}))
    assert "model unreadable" in line


def test_truth_verb_json_carries_observed_model_and_keeps_exit_zero(
    tmp_path, monkeypatch
):
    """The --json payload is an explicit key allowlist, so a field added to the
    resolver reaches nobody until it is added there too. Exit code is unchanged:
    this is a reporting field, never a gate."""
    from typer.testing import CliRunner

    from fno.agents import session_truth
    from fno.cli import app

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-1111-0000-0000-000000000001"
    _write_claude_transcript_with_model(tmp_path, cwd, sid, "glm-5.2", turns=2)
    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])

    real = session_truth.resolve_session_truth
    monkeypatch.setattr(
        session_truth,
        "resolve_session_truth",
        lambda handle, **kw: real(
            handle, resolve=_resolver(session), projects_root=tmp_path
        ),
    )

    result = CliRunner().invoke(app, ["agents", "truth", "w1", "--json"])

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["observed_model"] == {
        "kind": "observed",
        "model": "glm-5.2",
        "samples": 2,
    }


def test_observed_model_escalates_past_an_inconclusive_tail(tmp_path):
    """A bounded tail cannot claim absence.

    codex stamps the model once per TURN on `turn_context`, so one tool-heavy
    turn pushes it clean out of a fixed tail window (measured: a 622 KB rollout
    whose only turn_context sat at byte 126850). Reporting "no model yet" from
    a window we never looked past would call a healthy worker unhealthy -- the
    exact wrong diagnosis this reading exists to prevent.
    """
    from fno.agents.session_truth import _MODEL_TAIL_BYTES, observed_model

    path = tmp_path / "rollout.jsonl"
    context = json.dumps({"type": "turn_context", "payload": {"model": "gpt-5.6-sol"}})
    filler = json.dumps({"type": "event_msg", "payload": {"text": "x" * 900}})
    with path.open("w") as fh:
        fh.write(context + "\n")  # the only model record, at the very top
        for _ in range(_MODEL_TAIL_BYTES // len(filler) + 50):
            fh.write(filler + "\n")

    assert path.stat().st_size > _MODEL_TAIL_BYTES
    assert observed_model("codex", path) == {
        "kind": "observed",
        "model": "gpt-5.6-sol",
        "samples": 1,
    }
