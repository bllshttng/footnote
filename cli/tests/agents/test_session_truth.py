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
        # x-c1a3: a tag describes a TURN, and a turn stops being news once the
        # transcript has been silent past stalled_after_s. Only <promise> is a
        # turn OUTCOME and survives any age; watching/questions decay to stalled.
        ("assistant", '<watching reason="ci" pr="5" timeout="30m">', 3 * 3600, "stalled"),
        ("assistant", "anything ending in a question?", 99999, "stalled"),
        ("assistant", "proceed with the merge? (Y/n)", 3 * 3600, "stalled"),
        ("assistant", '<help reason="stuck" evidence="x">need a decision</help>', 99999, "stalled"),
        # an unknowable age is never evidence of staleness: the news survives
        ("assistant", '<watching reason="ci" pr="5" timeout="30m">', None, "watching"),
        ("assistant", "anything ending in a question?", None, "your-move"),
        # watching outranks promise when both appear (runtime parks watching)
        ("assistant", "<promise>done</promise> but also <watching pr=1>", 10, "watching"),
        # exact-marker discipline: a lookalike word is NOT a promise
        ("assistant", "we <promised> to fix it, still working", 10, "working"),
        # trailing USER turn clears a stale assistant signal -> worker's move
        ("user", "<promise>done</promise> (quoted by the operator)", 10, "working"),
        ("user", "here is your answer", 99999, "stalled"),
        # x-1182: a trailing option prompt is your-move immediately, not a
        # two-hour mtime wait. This is the exact tail that stalled a worker
        # for 50 minutes and read "working" the whole time.
        (
            "assistant",
            "Canonical main is protected. Create a dedicated worktree "
            "for x-884f and continue there? [Y/n]",
            3000,  # 50 minutes, well under STALLED_AFTER_S (2h)
            "your-move",
        ),
        ("assistant", "run the migration now? [y/N]", 10, "your-move"),
        ("assistant", "overwrite the file? (y/N)", 10, "your-move"),
        ("assistant", "proceed with the merge? (Y/n)", 10, "your-move"),
        ("assistant", "pick a target: [1/2/3]", 10, "your-move"),
        # mid-body mention is prose about a prompt, not a prompt -- must not
        # fire the option-prompt grammar
        (
            "assistant",
            "the worker asked [Y/n] and then kept going with more analysis",
            99999,
            "stalled",
        ),
        # a trailing user turn clears an option-prompt signal exactly as it
        # already clears "?" and <help> (the operator answered)
        ("user", "continue? [Y/n]", 99999, "stalled"),
        # a turn that IS a harness API error is a death signal at any age --
        # two workers held claims 45 minutes while every reader called them
        # working, keyed on an error tail read as activity.
        (
            "assistant",
            "API Error: The response stopped arriving. The response above may be incomplete.",
            45 * 60,
            "stalled",
        ),
        # another measured spelling of the family (429 quota, 238 of 416)
        ("assistant", "API Error: Request rejected (429)", 10, "stalled"),
        # the node's named negative control: a correctly watching session with
        # the SAME silence still reads live
        ("assistant", '<watching reason="ci" pr="1822" timeout="30m">', 45 * 60, "watching"),
        # anchoring, not substring: prose ABOUT an error is a live worker
        # (this node's own worker reads "api error" in every turn and must
        # never trip the arm)
        (
            "assistant",
            "I saw an API Error: rate limit exceeded in the log, so I retried",
            45 * 60,
            "working",
        ),
        (
            "assistant",
            "the plan documents the API Error tail classifier and its false-positive bound",
            45 * 60,
            "working",
        ),
        # content arms apply to assistant turns only: a trailing user turn
        # carrying error text means the operator pasted/quoted it
        ("user", "API Error: The response stopped arriving.", 10, "working"),
        # watching outranks the error arm when both appear in one turn
        (
            "assistant",
            '<watching pr=5> API Error: connection closed mid-response',
            10,
            "watching",
        ),
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


def test_observed_title_rides_the_truth_payload(tmp_path):
    """A Ctrl+R rename appends an agent-name record and fires no hook, so the
    tail reader is the only detector; the title is SERVED beside the label
    (``harness_title``) and never written into it."""
    from fno.agents.session_truth import resolve_session_truth

    sid = "11aa78de-2222-3333-4444-555566667777"
    p = tmp_path / f"{sid}.jsonl"
    p.write_text(
        "\n".join(
            [
                json.dumps({"type": "user", "message": {
                    "role": "user", "content": [{"type": "text", "text": "go"}]}}),
                json.dumps({"type": "agent-name", "agentName": "renamed-in-view"}),
                json.dumps({"type": "assistant", "message": {
                    "role": "assistant", "model": "glm",
                    "content": [{"type": "text", "text": "on it"}]}}),
            ]
        )
        + "\n"
    )
    session = SimpleNamespace(
        agent="claude", session_id=sid, cwd="/w", short_id=sid[:8], transcript_path=str(p)
    )
    result = resolve_session_truth("w1", resolve=_resolver(session), projects_root=tmp_path)
    assert result["harness_title"] == "renamed-in-view"


def test_resolve_without_a_title_reports_absence(tmp_path):
    """No agent-name record -> ``harness_title`` is None: absence renders as
    absence, never as the fno label or a fabricated default."""
    from fno.agents.session_truth import resolve_session_truth

    sid = "99fe1ba0-1111-2222-3333-444455556666"
    p = tmp_path / f"{sid}.jsonl"
    p.write_text(
        json.dumps({"type": "user", "message": {
            "role": "user", "content": [{"type": "text", "text": "go"}]}}) + "\n"
    )
    session = SimpleNamespace(
        agent="claude", session_id=sid, cwd="/w", short_id=sid[:8], transcript_path=str(p)
    )
    result = resolve_session_truth("w1", resolve=_resolver(session), projects_root=tmp_path)
    assert result["harness_title"] is None


def test_resolve_reads_worktree_transcript_your_move(tmp_path):
    """AC2-HP + integrates the resolver fix: session dispatched with canonical
    cwd, live transcript in the worktree dir, last turn ends in a question."""
    import os

    from fno.agents.session_truth import resolve_session_truth

    canonical = "/Users/bb16/code/footnote/footnote"
    worktree = "/Users/bb16/code/footnote/footnote/.claude/worktrees/x-a472"
    sid = "4ec8a08b-9fe7-4550-8e40-00c7fd4e600a"
    path = _write_claude_transcript(tmp_path, worktree, sid, ["Should I rebase onto main?"])
    # Fresh relative to now_s: expiry (x-c1a3) means a question tail only reads
    # your-move while the transcript is recent.
    os.utime(path, (2_000_000_000 - 60, 2_000_000_000 - 60))

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
# last_event_at + last_message: the absolute stamp and the last turn
#
# The pair makes a wedged worker legible: a row claiming `working` whose stamp
# is hours old, with the wait-line it actually ended on. Both come from the
# same tail read that classified the turn, and both are None on every unknown
# path - an unread transcript renders as unread, never as fresh.
# ---------------------------------------------------------------------------

def test_resolve_carries_last_event_stamp_and_last_message(tmp_path):
    import os

    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "abcdef12-4e20-0000-0000-000000000000"
    path = _write_claude_transcript(
        tmp_path, cwd, sid, ["first turn", ("user", "line one\nline two")]
    )
    os.utime(path, (1_700_000_000, 1_700_000_000))

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path, now_s=1_700_000_100.0
    )

    # The stamp is absolute UTC in the Z-suffix house format (the same shape
    # registry.py stamps last_message_at with) and agrees with the age by
    # construction: both come from the same epoch read.
    assert result["last_event_at"] == "2023-11-14T22:13:20Z"
    assert result["last_activity_age_s"] == 100
    # The LAST turn's text, whitespace collapsed.
    assert result["last_message"] == "line one line two"


def test_resolve_degrades_out_of_range_epoch_to_absent_stamp(tmp_path, monkeypatch):
    """An epoch beyond datetime range must not raise: the module contract says
    every read degrades, and one corrupt reading must not take down the whole
    `fno agents list` render (read.py calls this per row with no catch). A
    filesystem mtime cannot reach the danger zone (ns precision clamps at year
    2262); the reachable producer is the opencode DB, whose time_updated is
    divided by 1000 with no range guard, so a microsecond-scale row yields a
    year-55840 epoch."""
    from fno.agents import session_truth
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "abcdef12-4e23-0000-0000-000000000000"
    _write_claude_transcript(tmp_path, cwd, sid, ["turn"])

    monkeypatch.setattr(
        session_truth,
        "_transcript_age_s",
        lambda *a, **k: (1_700_000_000_000.0, 0.0, "mtime"),
    )

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path, now_s=1_700_000_100.0
    )

    assert result["last_event_at"] is None


def test_transcript_age_degrades_out_of_range_epoch_as_a_pair(monkeypatch, tmp_path):
    """The out-of-range guard must null BOTH halves of the pair, not just the
    stamp: a far-future epoch through `max(0.0, now - mtime)` yields a measured
    age of 0, which is a freshness claim about a reading that never happened -
    the exact stamp-says-absent / age-says-fresh disagreement the paired return
    exists to prevent. Reached through the opencode lane, the one reachable
    producer of an out-of-range epoch."""
    from fno.agents import session_truth

    monkeypatch.setattr(
        session_truth, "_opencode_activity_epoch", lambda sid, p: 1_700_000_000_000.0
    )

    class _RT:
        resolved = True
        kind = "opencode-db"
        transcript_path = str(tmp_path / "store.db")

    monkeypatch.setattr(
        "fno.provenance.resolver.resolve_transcript", lambda *a, **k: _RT()
    )

    assert session_truth._transcript_age_s("opencode", "s", "/c", None, None, None) == (
        None,
        None,
        None,
    )


def test_resolve_last_message_keeps_tool_marker_and_caps_length(tmp_path):
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "abcdef12-4e21-0000-0000-000000000000"
    d = tmp_path / cwd.replace("/", "-").replace(".", "-")
    d.mkdir(parents=True, exist_ok=True)
    (d / f"{sid}.jsonl").write_text(
        json.dumps(
            {
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "name": "Bash", "input": {}},
                        {"type": "text", "text": "Still growing " + "x" * 250},
                    ],
                },
            }
        )
        + "\n"
    )

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )

    msg = result["last_message"]
    # The compact tool marker is inline (the tool half of "what did it last
    # DO"), and a 270-char line is capped at 200 so one row cannot own a table.
    assert msg.startswith("[tool_use: Bash] Still growing")
    assert len(msg) == 200


def test_unknown_paths_leave_the_last_event_pair_null(tmp_path):
    """not-found and no-records both emit the full key set with both new keys
    None: an absent probe reading is never a fresh stamp or a message."""
    from fno.agents.session_truth import resolve_session_truth

    def miss(_handle):
        return None, []

    result = resolve_session_truth("nope", resolve=miss, projects_root=tmp_path)
    assert result["state"] == "unknown"
    assert result["last_event_at"] is None
    assert result["last_message"] is None

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "beadface-4e22-0000-0000-000000000000"
    _write_claude_transcript(tmp_path, cwd, sid, [])
    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )
    assert result["state"] == "unknown"
    assert result["last_event_at"] is None
    assert result["last_message"] is None


def test_truth_verb_json_carries_the_last_event_pair(tmp_path, monkeypatch):
    """The --json payload is an explicit key allowlist (see the observed_model
    test above): the resolver's fields reach nobody until _truth_payload copies
    them across."""
    from typer.testing import CliRunner

    from fno.agents import session_truth
    from fno.cli import app

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-2222-0000-0000-000000000001"
    _write_claude_transcript(tmp_path, cwd, sid, ["on the pytest run now"])
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
    payload = json.loads(result.stdout)
    assert payload["last_message"] == "on the pytest run now"
    assert payload["last_event_at"]  # absolute ISO8601, present on the wire
    # The wire names the instrument too: this fixture carries no timestamps,
    # so the reading rode the labelled mtime fallback.
    assert payload["last_activity_basis"] == "mtime"


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

    This is the diagnostic the whole five-variant shape exists for: a worker
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
    from fno.provenance.observed import _MODEL_TAIL_BYTES, observed_model

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
    # The age is fixed-width (4), so a short one carries leading spaces here
    # too - column alignment is worth a double space in prose.
    assert line == "truth 569d8d39: working on glm-5.2 (active, last activity   6s ago)"


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
    assert line == "truth 569d8d39: working (active, last activity   6s ago)"


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
    from fno.provenance.observed import _MODEL_TAIL_BYTES, observed_model

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


# ---------------------------------------------------------------------------
# --handles: one process answers N handles (x-0d93)
#
# The probe's real work is 0.83 ms and its delivery was 780 ms of interpreter
# cold start, paid once per roster row per daemon sweep. These pin the batch
# contract the Rust side now depends on: a keyed object, an always-zero exit,
# and one registry read for the whole batch.
# ---------------------------------------------------------------------------

def test_truth_batch_keys_every_handle_and_exits_zero_with_a_miss(
    tmp_path, monkeypatch
):
    """Two resolve, one does not. The miss rides its own entry, never the exit
    code -- a batch has no single exit code to carry."""
    from typer.testing import CliRunner

    from fno.agents import session_truth
    from fno.cli import app

    cwd = "/Users/bb16/code/footnote/footnote"
    sids = {
        "a": "0badc0de-3333-0000-0000-00000000000a",
        "b": "0badc0de-3333-0000-0000-00000000000b",
    }
    for name, sid in sids.items():
        _write_claude_transcript(tmp_path, cwd, sid, [f"still going on {name}"])

    real = session_truth.resolve_session_truth

    def routed(handle, **kw):
        sid = sids.get(handle)
        if sid is None:
            return real(handle, resolve=lambda _h: (None, []), projects_root=tmp_path)
        session = SimpleNamespace(
            agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8]
        )
        return real(handle, resolve=_resolver(session), projects_root=tmp_path)

    monkeypatch.setattr(session_truth, "resolve_session_truth", routed)

    result = CliRunner().invoke(
        app, ["agents", "truth", "--handles", "a,b,c", "--json"]
    )

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert set(payload) == {"a", "b", "c"}
    assert payload["a"]["last_message"] == "still going on a"
    assert payload["b"]["last_message"] == "still going on b"
    assert payload["c"]["state"] == "unknown"
    assert payload["c"]["reason"]  # the miss explains itself in its own entry


def test_truth_batch_and_single_read_the_same_handle_identically(tmp_path, monkeypatch):
    """ONE reader, asserted on the whole payload rather than on one field.

    This is the constraint the batch exists to preserve. `state` and
    `observed_model` must come from the same reader, so the Rust list emitter
    reports the identical reading the Python one does -- see `TruthProbe` in
    `crates/fno-agents/src/claude_ask.rs`. A batch that answered even slightly
    differently from the single form would be the second reader a Rust port was
    rejected for growing.

    Compared as full dicts, not as a state string: a check that pins one field
    passes while every other field drifts.
    """
    from typer.testing import CliRunner

    from fno.agents import session_truth
    from fno.cli import app

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-4444-0000-0000-000000000001"
    _write_claude_transcript_with_model(tmp_path, cwd, sid, "glm-5.3", turns=3)
    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])

    real = session_truth.resolve_session_truth
    monkeypatch.setattr(
        session_truth,
        "resolve_session_truth",
        lambda handle, **kw: real(
            handle, resolve=_resolver(session), projects_root=tmp_path
        ),
    )

    runner = CliRunner()
    single = json.loads(
        runner.invoke(app, ["agents", "truth", "w1", "--json"]).stdout
    )
    batch = json.loads(
        runner.invoke(app, ["agents", "truth", "--handles", "w1", "--json"]).stdout
    )["w1"]

    # The age advances between two reads by construction; it is the one field a
    # parity check must exclude rather than the one field it should check.
    drifts = {"last_activity_age_s"}
    assert {k: v for k, v in batch.items() if k not in drifts} == {
        k: v for k, v in single.items() if k not in drifts
    }


def test_batch_resolver_runs_one_discovery_scan_for_every_handle(monkeypatch):
    """The cost inside the cost, and the one that actually dominated.

    A NAME handle misses the registry fast path in `resolve_or_suggest` and
    falls through to `discover_live_sessions`, which sweeps every process on the
    box with psutil and globs the transcript stores. Profiled on the live
    roster: 483 ms a call, run once per handle, so twelve handles spent 5.8 of
    their 8.3 seconds in it. One process is not cheap on its own -- it is cheap
    once the per-handle reads inside it are hoisted. Twelve handles went from
    5.72 s to 1.56 s, and the full 31-row roster answers in 3.94 s.
    """
    from fno.agents import cli as agents_cli
    from fno.agents import discover as discover_mod

    scans = []
    registry_reads = []

    def counting_discover(**kwargs):
        scans.append(kwargs.get("resolve_metadata", True))
        return []

    def counting_registry(_path=None):
        registry_reads.append(1)
        return []

    monkeypatch.setattr(discover_mod, "discover_live_sessions", counting_discover)
    monkeypatch.setattr(discover_mod, "_discover_from_registry", counting_registry)

    resolve = agents_cli._batch_resolver()
    for handle in ["a", "b", "c", "d", "e"]:
        resolve(handle)

    # Two scan shapes exist on this path (bare, then full). Each is paid ONCE
    # for the whole batch rather than once per handle.
    assert len(scans) == 2, f"one scan per shape, got {scans}"
    assert sorted(scans) == [False, True]
    assert len(registry_reads) == 1, "the registry is read once for the batch"


def test_batch_resolver_falls_back_when_the_hoisted_read_fails(monkeypatch):
    """A hoist must never make a batch LESS able to resolve than a single call.

    When the hoisted registry read raises, the resolver passes `registry_rows=None`
    and `resolve_or_suggest` reads for itself, exactly as the single-handle path
    does.
    """
    from fno.agents import cli as agents_cli
    from fno.agents import discover as discover_mod

    def boom(_path=None):
        raise RuntimeError("unreadable registry")

    seen = {}

    def fake_resolve_or_suggest(handle, **kwargs):
        seen.update(kwargs)
        return None, []

    monkeypatch.setattr(discover_mod, "_discover_from_registry", boom)
    monkeypatch.setattr(discover_mod, "resolve_or_suggest", fake_resolve_or_suggest)

    agents_cli._batch_resolver()("w1")

    assert seen["registry_rows"] is None, "a failed hoist degrades to the per-call read"
    assert seen["require_alive"] is False


def test_truth_batch_refuses_both_a_positional_handle_and_handles():
    """Two spellings of one question. Exit 2 (usage), nothing on stdout."""
    from typer.testing import CliRunner

    from fno.cli import app

    result = CliRunner().invoke(app, ["agents", "truth", "w1", "--handles", "a,b"])

    assert result.exit_code == 2, result.output
    assert result.stdout.strip() == ""


def test_truth_batch_reads_the_registry_once_for_every_handle(monkeypatch):
    """`load_registry` per handle was the cost inside the cost. One read now
    serves the whole batch, and the three-key match is unchanged."""
    from fno.agents import cli as agents_cli

    rows = [
        SimpleNamespace(name="alpha", harness_session_id="sid-alpha", short_id="aaaa"),
        SimpleNamespace(name="beta", harness_session_id="sid-beta", short_id="bbbb"),
    ]
    reads = []

    def counting_load_registry():
        reads.append(1)
        return rows

    monkeypatch.setattr("fno.agents.registry.load_registry", counting_load_registry)
    monkeypatch.setattr(
        "fno.agents.reachability.registry_falsifier", lambda row: f"gone:{row.name}"
    )

    # Matched on name, session id AND short id, exactly as the single form does.
    out = agents_cli._registry_falsifiers(["alpha", "sid-beta", "bbbb", "nobody"])

    assert len(reads) == 1
    assert out == {
        "alpha": "gone:alpha",
        "sid-beta": "gone:beta",
        "bbbb": "gone:beta",
        "nobody": None,
    }


def test_resolve_session_truth_peer_mail_does_not_clear_question(tmp_path):
    """Trailing peer mail turns must not clear assistant question/help state."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/tmp/proj"
    sid = "sess-truth-peer"
    turns = [
        ("user", "Start working"),
        ("assistant", "Should I use postgres or sqlite?"),
        ("user", '<fno_mail from="cc-peer001" harness="claude">here is a status update</fno_mail>'),
    ]
    _write_claude_transcript(tmp_path, cwd, sid, turns)
    sess = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, transcript_path=None)

    truth = resolve_session_truth("w1", resolve=lambda h: (sess, []), projects_root=tmp_path, now_s=None)
    assert truth["state"] == "your-move"
    assert "<fno_mail" in truth["last_message"]


# ---------------------------------------------------------------------------
# last_activity_basis: the age names its instrument (x-54cf)
#
# The file mtime OVERSTATES liveness, never understates it: trailing
# untimestamped records (last-prompt, cost state) keep touching the file while
# the conversation is silent - measured never negative, median +20 minutes,
# maximum +240 hours over 311 claude transcripts. The age now comes from the
# newest timestamped entry in the tail the classifier already read, and every
# fallback names itself.
# ---------------------------------------------------------------------------

def _write_skewed_transcript(
    projects_root: Path, cwd: str, sid: str, now_s: float, entry_age_s: float
) -> Path:
    """Newest timestamped entry ``entry_age_s`` old, mtime fresh.

    Produced the way production produces the skew: dated turns, then an
    untimestamped trailing record that touches the file and dates nothing,
    then a fresh utime.
    """
    import os
    from datetime import datetime, timezone

    slug = cwd.replace("/", "-").replace(".", "-")
    d = projects_root / slug
    d.mkdir(parents=True, exist_ok=True)
    path = d / f"{sid}.jsonl"
    stamp = datetime.fromtimestamp(now_s - entry_age_s, tz=timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    lines = [
        json.dumps({"type": "user", "timestamp": stamp, "message": {
            "role": "user", "content": [{"type": "text", "text": "go"}]}}),
        json.dumps({"type": "assistant", "timestamp": stamp, "message": {
            "role": "assistant", "content": [{"type": "text", "text": "grinding on"}]}}),
        # The record class that lies to mtime: it freshens the file stat and
        # carries no timestamp the conversation can be aged by.
        json.dumps({"type": "last-prompt"}),
    ]
    path.write_text("\n".join(lines) + "\n")
    os.utime(path, (now_s, now_s))
    return path


def test_skew_reports_the_entry_age_not_the_file_age(tmp_path):
    """mtime reads 0s, the newest entry reads 1h: the entry wins and the
    reading names its instrument."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-54cf-0000-0000-000000000001"
    now_s = 2_000_000_000.0
    _write_skewed_transcript(tmp_path, cwd, sid, now_s, entry_age_s=3600)

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path, now_s=now_s
    )

    assert abs(result["last_activity_age_s"] - 3600) <= 5
    assert result["last_activity_basis"] == "last-entry"
    assert result["state"] == "working"  # 1h is inside the 2h stalled window


def test_positive_control_a_fresh_entry_reads_fresh_in_the_same_run(tmp_path):
    """Same reader, 10s-old entry, same run: the age tracks the input. A
    reader that returns a plausible number for every input proves nothing."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-54cf-0000-0000-000000000002"
    now_s = 2_000_000_000.0
    _write_skewed_transcript(tmp_path, cwd, sid, now_s, entry_age_s=10)

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path, now_s=now_s
    )

    assert result["last_activity_age_s"] < 60
    assert result["last_activity_basis"] == "last-entry"


def test_a_tail_without_timestamps_falls_back_and_names_mtime(tmp_path):
    import os

    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "abcdef12-54cf-0000-0000-000000000003"
    path = _write_claude_transcript(tmp_path, cwd, sid, ["pre-timestamp turn"])
    os.utime(path, (1_700_000_000, 1_700_000_000))

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path, now_s=1_700_000_100.0
    )

    # The one reading that can still overstate liveness is labelled as such.
    assert result["last_activity_age_s"] == 100
    assert result["last_activity_basis"] == "mtime"
    assert result["last_event_at"] == "2023-11-14T22:13:20Z"


def test_an_unrenderable_tail_stamp_falls_to_the_fallback_as_one_pair(tmp_path, monkeypatch):
    """A tail epoch that cannot render as a stamp never becomes the epoch:
    the age and the stamp degrade together to the labelled fallback, the same
    paired discipline _transcript_age_s applies to its own reads."""
    import os

    from fno.agents import session_truth
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "abcdef12-54cf-0000-0000-000000000009"
    path = _write_claude_transcript(tmp_path, cwd, sid, ["turn"])
    os.utime(path, (1_700_000_000, 1_700_000_000))

    real = session_truth._record_stamp_epoch

    def huge(_ts):
        return 1.0e12  # beyond datetime's range: renders as neither stamp nor age

    monkeypatch.setattr(session_truth, "_record_stamp_epoch", huge)
    # Keep the parser honest for the control: the real parser could never
    # produce this value (its ISO input caps at year 9999), which is why the
    # guard is exercised through the seam.
    assert real("2026-09-09T20:00:00Z") is not None

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path, now_s=1_700_000_100.0
    )

    assert result["last_activity_age_s"] == 100
    assert result["last_activity_basis"] == "mtime"
    assert result["last_event_at"] == "2023-11-14T22:13:20Z"


def test_newest_entry_epoch_reads_the_whole_file_when_asked(tmp_path):
    """tail_bytes=None is the adopt stamp's shape: the window can never be
    narrower than the tail truth's own read covers, or one transcript ages by
    two instruments."""
    from datetime import datetime, timezone

    from fno.agents.session_truth import newest_entry_epoch

    path = tmp_path / "t.jsonl"
    stamp = "2026-01-01T01:00:00Z"
    filler = json.dumps({"type": "progress"})  # untimestamped, dates nothing
    count = 300 * 1024 // (len(filler) + 1) + 10
    lines = [json.dumps({"type": "assistant", "timestamp": stamp, "message": {}})]
    lines.extend([filler] * count)
    path.write_text("\n".join(lines) + "\n")
    assert path.stat().st_size > 256 * 1024
    # The bounded window sees only untimestamped records -> None.
    assert newest_entry_epoch(path) is None
    expected = datetime(2026, 1, 1, 1, 0, tzinfo=timezone.utc).timestamp()
    assert newest_entry_epoch(path, tail_bytes=None) == expected


def test_opencode_age_names_the_db_basis(tmp_path, monkeypatch):
    """opencode keeps the MAX(time_updated) reading and its own basis name."""
    from fno.agents import session_truth

    monkeypatch.setattr(
        session_truth, "_opencode_activity_epoch", lambda sid, p: 1_700_000_000.0
    )

    class _RT:
        resolved = True
        kind = "opencode-db"
        transcript_path = str(tmp_path / "store.db")

    monkeypatch.setattr(
        "fno.provenance.resolver.resolve_transcript", lambda *a, **k: _RT()
    )

    epoch, age, basis = session_truth._transcript_age_s(
        "opencode", "s", "/c", None, None, 1_700_000_100.0
    )
    assert (epoch, age, basis) == (1_700_000_000.0, 100.0, "opencode-db")


def test_claude_record_carries_its_timestamp():
    """The claude parser keeps the record's timestamp the way the codex parser
    already does, so the age can come from the tail instead of a stat."""
    from fno.agents.peek import _parse_claude_record

    rec = {
        "type": "assistant",
        "timestamp": "2026-09-09T20:00:00Z",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
    }
    assert _parse_claude_record(rec).timestamp == "2026-09-09T20:00:00Z"
    del rec["timestamp"]
    assert _parse_claude_record(rec).timestamp is None


def test_newest_entry_epoch_reads_the_newest_timestamped_line(tmp_path):
    from datetime import datetime, timezone

    from fno.agents.session_truth import newest_entry_epoch

    path = tmp_path / "t.jsonl"
    path.write_text(
        "\n".join(
            [
                json.dumps({"type": "user", "timestamp": "2026-01-01T00:00:00Z",
                            "message": {}}),
                json.dumps({"type": "assistant", "timestamp": "2026-01-01T01:00:00Z",
                            "message": {}}),
                # Untimestamped trailing record and a torn line: neither dates
                # anything, neither breaks the read.
                json.dumps({"type": "last-prompt"}),
                '{"type":"assist',
            ]
        )
        + "\n"
    )
    expected = datetime(2026, 1, 1, 1, 0, tzinfo=timezone.utc).timestamp()
    assert newest_entry_epoch(path) == expected


def test_newest_entry_epoch_none_without_timestamps_or_file(tmp_path):
    from fno.agents.session_truth import newest_entry_epoch

    bare = tmp_path / "bare.jsonl"
    bare.write_text(json.dumps({"type": "last-prompt"}) + "\n")
    assert newest_entry_epoch(bare) is None
    assert newest_entry_epoch(tmp_path / "absent.jsonl") is None


def test_render_truth_names_the_basis():
    from fno.agents.session_truth import render_truth

    line = render_truth(
        {"handle": "w1", "state": "stalled", "reason": None,
         "last_activity_age_s": 48 * 3600, "last_activity_basis": "last-entry",
         "session_id": "s", "suggestions": []}
    )
    assert line.endswith("by last-entry)")
    assert "2d" in line


# ---------------------------------------------------------------------------
# provider_refusal: the quota refusal the last assistant turn carries (x-e594)
#
# Ten workers died on usage-limit 429s and every one read `quiet` or `writing`,
# because the renderers never consulted the refusal text already on the row.
# The truth reading is where the taxonomy is applied once, and both status
# lanes consume the field from here.
# ---------------------------------------------------------------------------

_QUOTA_BODY = (
    "API Error: Request rejected (429) · [1308][Usage limit reached for 5 hour. "
    "Your limit will reset at 2026-09-11 19:37:55]"
)


def test_ac1_quota_refusal_rides_the_truth_payload(tmp_path):
    """AC1-HP: the measured corpse's tail - last assistant record is the
    usage-limit 429 - classifies to provider_4xx_quota on the reading."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-e594-0000-0000-000000000001"
    _write_claude_transcript(tmp_path, cwd, sid, ["on it", _QUOTA_BODY])

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )
    assert result["provider_refusal"] == "provider_4xx_quota"


@pytest.mark.parametrize(
    "turns",
    [
        # the same text in a USER turn: a mailed or pasted refusal is prose
        # about a cap, never the worker's own refusal
        ["on it", _QUOTA_BODY, ("user", _QUOTA_BODY)],
        # the quota words start after the 120-char lead: prose about a cap
        # deeper in a turn is not a refusal to classify on
        ["on it", "word " * 40 + _QUOTA_BODY],
        # a finished worker is an outcome, not a refusal
        ["<promise>MISSION COMPLETE: shipped</promise>"],
    ],
)
def test_ac2_non_qualifying_tails_leave_provider_refusal_null(tmp_path, turns):
    """AC2-ERR: role, lead-window and done each keep the field null."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-e594-0000-0000-000000000002"
    _write_claude_transcript(tmp_path, cwd, sid, turns)

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )
    assert result["provider_refusal"] is None


def test_ac2_unknown_paths_carry_provider_refusal_none(tmp_path):
    """AC2-ERR: every unknown path emits the key as None, so a Rust probe
    parsing an older body and a fresh one agree on the absent case."""
    from fno.agents.session_truth import resolve_session_truth

    def miss(_handle):
        return None, []

    result = resolve_session_truth("nope", resolve=miss, projects_root=tmp_path)
    assert result["provider_refusal"] is None


def test_a_raising_refusal_classifier_never_breaks_the_liveness_read(tmp_path, monkeypatch):
    """`resolve_session_truth` is documented never to raise, and every liveness
    surface reads it. So the refusal classification is a reporting field that
    must fail to None rather than take the whole read down with it - the same
    rule `observed_model` states for itself."""
    import fno.recovery as recovery
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-e594-0000-0000-0000000000ff"
    _write_claude_transcript(tmp_path, cwd, sid, ["still going"])
    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])

    def boom(*_a, **_k):
        raise RuntimeError("taxonomy blew up")

    monkeypatch.setattr(recovery, "classify_worker_refusal", boom)

    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )
    assert result["provider_refusal"] is None
    assert result["state"] != "unknown"


# ---------------------------------------------------------------------------
# an API-error tail is a death signal, and the reason says which
# ---------------------------------------------------------------------------

def test_ac5_error_tail_resolves_stalled_with_reason(tmp_path):
    """AC5-HP: through the resolve path, the errored session reads stalled and
    the reason names the error arm -- "stalled" alone would read as "silent"
    and send a debugger hunting for a timeout that does not exist."""
    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-e594-0000-0000-000000000250"
    _write_claude_transcript(
        tmp_path,
        cwd,
        sid,
        ["API Error: The response stopped arriving. The response above may be incomplete."],
    )

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )
    assert result["state"] == "stalled"
    assert result["reason"] == "api-error-tail"


def test_ac6_plain_silence_keeps_reason_null(tmp_path):
    """AC6-EDGE: stalled from plain silence carries reason None -- the reason
    marks the error path only and never becomes a second name for silence."""
    import os
    import time as _time

    from fno.agents.session_truth import resolve_session_truth

    cwd = "/Users/bb16/code/footnote/footnote"
    sid = "0badc0de-e594-0000-0000-000000000251"
    path = _write_claude_transcript(tmp_path, cwd, sid, ["still grinding on the parser"])
    # Age the mtime past STALLED_AFTER_S so this is the silence arm, not freshness
    old = _time.time() - 3 * 3600
    os.utime(path, (old, old))

    session = SimpleNamespace(agent="claude", session_id=sid, cwd=cwd, short_id=sid[:8])
    result = resolve_session_truth(
        "w1", resolve=_resolver(session), projects_root=tmp_path
    )
    assert result["state"] == "stalled"
    assert result["reason"] is None


def test_ac7_tool_use_then_error_text_does_not_fire(tmp_path):
    """AC7-EDGE: a turn that did tool work and then errored flattens to
    ``[tool_use: name] API Error: ...`` -- the error text is off position 0,
    so the anchor does not fire and the tail reads working. Pins the anchor
    against a future _extract_text refactor that moves the error text."""
    import json as _json

    from fno.agents.peek import _parse_claude_record
    from fno.agents.session_truth import classify_tail

    rec = _json.dumps(
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}},
                    {
                        "type": "text",
                        "text": "API Error: The response stopped arriving.",
                    },
                ],
            },
        }
    )
    record = _parse_claude_record(_json.loads(rec))
    assert record is not None
    assert classify_tail(record.role, record.text, 45 * 60) == "working"
