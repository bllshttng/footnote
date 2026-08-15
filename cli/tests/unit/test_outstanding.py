"""Tests for `fno outstanding` - the operator-facing read of what is piled up.

Two legs, both over stores that already exist: unharvested carve-outs and open
operator questions. The verb exists because the carve-out ledger reached 39 rows
over 29 days with nothing ever asking a human to look at it - `consume_carveouts`
was wired the whole time, but a wire nobody is told to pull is not a surface.

Every assertion here names a POSITIVE marker rather than an absence. An
absence has two explanations (the real outcome, or the instrument never ran)
and a test built on one cannot tell them apart; test_hook_block_positive_control
is the pair that closes that gap for the silent-on-zero case.
"""
from __future__ import annotations

import json
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.outstanding.cli import outstanding_app
from fno.outstanding.core import RENDER_CAP

runner = CliRunner()


def _write_carveouts(root: Path, rows: list[dict]) -> Path:
    ledger = root / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        "".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8"
    )
    return ledger


def _carveout(cid: str, kind: str, ts: str = "2026-07-14T04:27:55Z") -> dict:
    return {
        "id": cid,
        "ts": ts,
        "session_id": "sess-a",
        "kind": kind,
        "priority": None,
        "need": None,
        "description": f"{cid} description",
        "truncated": False,
    }


@pytest.fixture()
def root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A hermetic canonical root; both legs resolve through it."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("FNO_AGENT_SELF", raising=False)
    monkeypatch.delenv("FNO_BG", raising=False)
    # Presence resolves through classify_presence, whose documented default is
    # "away" when no human signal is present - correct for a cron/script, and
    # what a bare pytest process looks like. Declare the attended case here so
    # each test states the presence it is exercising instead of inheriting it.
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    # resolve_repo_root is @cache'd and is warmed with the REAL worktree root
    # before this fixture runs. Without the clear, session resolution reads the
    # live target-state.md instead of this sandbox, and the ownership tests
    # would pass on the real session id rather than the one they set.
    import fno.paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    return tmp_path


# --- 2.1 both legs -----------------------------------------------------------

def test_both_legs_report_a_count_each(root: Path):
    _write_carveouts(
        root,
        [
            _carveout("cv-1", "oos-bug"),
            _carveout("cv-2", "oos-bug"),
            _carveout("cv-3", "deferred"),
        ],
    )
    assert runner.invoke(outstanding_app, ["ask", "should the gate refuse?"]).exit_code == 0
    assert runner.invoke(outstanding_app, ["ask", "which base do we rebase on?"]).exit_code == 0

    result = runner.invoke(outstanding_app, [])
    assert result.exit_code == 0, result.output

    # The carve-out leg names the split, not just a total: only `deferred`
    # blocks a node close, so a bare total hides which rows have a gate.
    assert "3 carve-out" in result.output
    assert "2 oos-bug" in result.output
    assert "1 deferred" in result.output
    assert "2 open question" in result.output


def test_carveout_leg_names_the_verb_that_clears_it(root: Path):
    _write_carveouts(root, [_carveout("cv-1", "oos-bug")])
    result = runner.invoke(outstanding_app, [])
    assert result.exit_code == 0
    # The whole fix for "nothing clears them": the ledger sat 29 days because
    # nobody was ever told what to run.
    assert "fno retro sweep-carveouts" in result.output


def test_carveout_leg_reports_the_age_of_the_oldest_row(root: Path):
    """Pins the DATE, not the word "oldest".

    The label is emitted for any row carrying a ts, so an `or "oldest" in ...`
    disjunct stays green even if the date slice regressed. Assert the thing
    measured, which is this file's own stated rule.
    """
    _write_carveouts(
        root,
        [
            _carveout("cv-old", "oos-bug", ts="2026-01-01T00:00:00Z"),
            _carveout("cv-new", "oos-bug", ts="2026-08-01T00:00:00Z"),
        ],
    )
    result = runner.invoke(outstanding_app, [])
    assert result.exit_code == 0
    assert "oldest 2026-01-01" in result.output
    # The newer row's date must NOT be the one reported as oldest.
    assert "oldest 2026-08-01" not in result.output


# --- 2.2 an unreadable ledger is a stated failure ----------------------------

def test_unreadable_ledger_is_a_stated_failure_not_silence(root: Path):
    ledger = root / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    # Present but undecodable. read_carveouts raises rather than returning []
    # for exactly this case; the new verb must not re-introduce the masquerade.
    ledger.write_bytes(b"\xff\xfe not utf-8 at all\n")

    result = runner.invoke(outstanding_app, [])

    assert result.exit_code != 0
    # Positive marker in the FAILURE text, pinned to the thing measured.
    assert "cannot read" in result.output or "failed to read" in result.output
    # And never the success line: a failed read reported as "nothing
    # outstanding" is the exact absence-as-success trap.
    assert "nothing outstanding" not in result.output.lower()


# --- 2.3 ask then clear ------------------------------------------------------

def test_ask_clear_round_trip_and_idempotence(root: Path):
    asked = runner.invoke(outstanding_app, ["ask", "do we widen the fold window?"])
    assert asked.exit_code == 0, asked.output
    qid = asked.stdout.strip().splitlines()[-1].strip()
    assert qid, "ask must print the new question id on stdout"

    listed = runner.invoke(outstanding_app, ["--json"])
    assert listed.exit_code == 0
    payload = json.loads(listed.stdout)
    assert [q["id"] for q in payload["questions"]] == [qid]

    cleared = runner.invoke(outstanding_app, ["clear", qid, "--answer", "yes, widen it"])
    assert cleared.exit_code == 0, cleared.output
    assert "1" in cleared.stdout

    after = json.loads(runner.invoke(outstanding_app, ["--json"]).stdout)
    assert after["questions"] == []

    # Idempotent: a second clear, and an id that was never open, are exit-0
    # no-ops that report a count of 0 rather than failing.
    again = runner.invoke(outstanding_app, ["clear", qid])
    assert again.exit_code == 0
    assert "0" in again.stdout

    unknown = runner.invoke(outstanding_app, ["clear", "q-neverexisted"])
    assert unknown.exit_code == 0
    assert "0" in unknown.stdout


def test_ask_records_the_answer_text_on_clear(root: Path):
    qid = runner.invoke(outstanding_app, ["ask", "which lane?"]).stdout.strip().splitlines()[-1]
    runner.invoke(outstanding_app, ["clear", qid, "--answer", "the codex lane"])
    lines = [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    closed = [e for e in lines if e["type"] == "operator_question_closed"]
    assert len(closed) == 1
    assert closed[0]["data"]["answer"] == "the codex lane"
    assert closed[0]["data"]["question_id"] == qid


def test_clear_with_answer_emits_operator_decision(root: Path):
    """AC3-HP: an answered close records the decision, not just the closure.

    The closed event stays; the decision event lands beside it carrying the
    recovery fields. A close with NO answer is a withdrawal and must not
    mint a decision.
    """
    asked = runner.invoke(
        outstanding_app, ["ask", "fold or migrate?", "--node", "x-7d94"]
    )
    qid = asked.stdout.strip().splitlines()[-1]
    cleared = runner.invoke(outstanding_app, ["clear", qid, "--answer", "fold"])
    assert cleared.exit_code == 0, cleared.output

    lines = [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    decisions = [e for e in lines if e["type"] == "operator_decision"]
    assert len(decisions) == 1, "an answered close must emit exactly one decision"
    data = decisions[0]["data"]
    assert data["decision"] == "fold"
    assert data["question_id"] == qid
    assert data["question"] == "fold or migrate?"
    assert data["subject"] == "x-7d94"
    assert data["decision_id"].startswith("d-")
    assert data["authority_source"] == "operator"
    assert data["asked_at"], "asked_at must inherit the question's timestamp"
    assert data["decided_by"]

    # A withdrawal (no --answer) decides nothing: positive control is the
    # closed event itself, the decision count stays at one from the ask above.
    qid2 = runner.invoke(outstanding_app, ["ask", "second question?"]).stdout.strip().splitlines()[-1]
    runner.invoke(outstanding_app, ["clear", qid2])
    lines = [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    assert len([e for e in lines if e["type"] == "operator_decision"]) == 1
    assert len([e for e in lines if e["type"] == "operator_question_closed"]) == 2


def test_unrelated_journal_volume_does_not_slow_the_read(root: Path):
    """The shared journal is append-only and never rotated.

    Parsing every line put the hook's 3s bound in reach, and that bound firing
    does not surface an error - the block just vanishes and the operator reads
    "nothing outstanding". Asserts the positive outcome (the question is still
    found among 20k unrelated rows) plus a wall-clock ceiling.
    """
    qid = runner.invoke(outstanding_app, ["ask", "buried under noise?"]).stdout.strip().splitlines()[-1]
    events = root / ".fno" / "events.jsonl"
    noise = json.dumps(
        {"ts": "2026-08-01T00:00:00Z", "type": "phase_transition", "source": "target",
         "data": {"phase": "do", "nonce": "x" * 32, "session_id": "s", "gate_bearing": False}}
    )
    with events.open("a", encoding="utf-8") as fh:
        for _ in range(20_000):
            fh.write(noise + "\n")

    started = time.monotonic()
    result = runner.invoke(outstanding_app, ["--json"])
    elapsed = time.monotonic() - started

    assert result.exit_code == 0, result.output
    assert [q["id"] for q in json.loads(result.stdout)["questions"]] == [qid]
    assert elapsed < 1.5, f"read took {elapsed:.2f}s over 20k unrelated rows"


def test_a_malformed_events_line_is_skipped_never_raised(root: Path):
    qid = runner.invoke(outstanding_app, ["ask", "still readable?"]).stdout.strip().splitlines()[-1]
    events = root / ".fno" / "events.jsonl"
    with events.open("a", encoding="utf-8") as fh:
        fh.write("{not json at all\n")

    result = runner.invoke(outstanding_app, ["--json"])
    assert result.exit_code == 0, result.output
    assert [q["id"] for q in json.loads(result.stdout)["questions"]] == [qid]


# --- 3rd leg: the capture fold (x-7d94 task 1.1) -----------------------------

def _write_inbox(root: Path, lines: list[str]) -> Path:
    inbox = root / "internal" / "fno" / "backlog" / "inbox.md"
    inbox.parent.mkdir(parents=True, exist_ok=True)
    inbox.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return inbox


def _capture_event(fu_id: str, ts: str) -> str:
    return json.dumps(
        {
            "ts": ts,
            "type": "capture_add",
            "source": "backlog",
            "data": {"session_id": "manual", "fu_id": fu_id, "title": "t"},
        }
    )


@pytest.fixture()
def capture_roots(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> "tuple[Path, Path]":
    """A hermetic THIS project plus one sibling project, both known to the
    machine-wide graph so the fold finds the sibling's inbox."""
    this = tmp_path / "this"
    other = tmp_path / "other"
    for p in (this, other):
        p.mkdir(parents=True)
    monkeypatch.setenv("FNO_REPO_ROOT", str(this))
    import fno.paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()
    (this / ".fno").mkdir(exist_ok=True)
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "x-0001", "title": "a", "cwd": str(this)},
                    {"id": "x-0002", "title": "b", "cwd": str(other)},
                ]
            }
        )
        + "\n"
    )
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", graph)
    monkeypatch.setattr(gs, "GRAPH_JSON", graph)
    return this, other


def test_capture_leg_folds_every_project_and_renders_true_count(capture_roots):
    """AC1-HP: three legs; the capture leg renders Showing N of M, oldest D
    days, with M folded across projects."""
    this, other = capture_roots
    _write_inbox(this, ["- [ ] fu-aaaaaa - alpha (p1)", "- [ ] fu-bbbbbb - beta"])
    _write_inbox(other, ["- [ ] fu-cccccc - gamma", "- [x] fu-dddddd - done -> ab-1"])

    res = runner.invoke(outstanding_app, [])
    assert res.exit_code == 0, res.output
    assert "3 captures" in res.output, res.output
    assert "Showing 3 of 3" in res.output, res.output

    as_json = runner.invoke(outstanding_app, ["--json"])
    payload = json.loads(as_json.stdout)
    assert payload["captures"]["total"] == 3
    assert payload["captures"]["by_project"][this.name] == 2
    assert payload["captures"]["by_project"][other.name] == 2 - 1


def test_capture_leg_reports_oldest_age_from_capture_add_events(capture_roots):
    """The inbox markdown carries no dates; age comes from capture_add in the
    events journal."""
    this, other = capture_roots
    _write_inbox(this, ["- [ ] fu-aaaaaa - alpha", "- [ ] fu-bbbbbb - beta"])
    events = this / ".fno" / "events.jsonl"
    events.parent.mkdir(exist_ok=True)
    old = (datetime.now(timezone.utc) - timedelta(days=62)).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    events.write_text(
        _capture_event("fu-aaaaaa", old) + "\n" + _capture_event("fu-bbbbbb", "2026-08-14T00:00:00Z") + "\n",
        encoding="utf-8",
    )

    res = runner.invoke(outstanding_app, [])
    assert res.exit_code == 0, res.output
    assert "oldest 62 days" in res.output, res.output


def test_unreadable_inbox_contributes_zero_and_never_raises(capture_roots):
    """AC2-EDGE: a missing/unreadable sibling inbox contributes zero captures
    and the other projects still report."""
    this, other = capture_roots
    _write_inbox(this, ["- [ ] fu-aaaaaa - alpha"])
    # other/ has no inbox at all; also plant a graph cwd that does not exist.
    import fno.graph._constants as gc
    import json as _json

    graph = gc.GRAPH_JSON
    data = _json.loads(graph.read_text())
    data["entries"].append({"id": "x-dead", "title": "d", "cwd": "/nonexistent/project"})
    graph.write_text(_json.dumps(data))

    res = runner.invoke(outstanding_app, [])
    assert res.exit_code == 0, res.output
    assert "1 capture" in res.output, res.output


def test_capture_render_caps_rows_but_keeps_the_true_count(capture_roots):
    """Ruling 2: never the full list; the count beside the cap is the honesty."""
    this, _ = capture_roots
    _write_inbox(
        this, [f"- [ ] fu-{i:06x} - item {i}" for i in range(10)]
    )

    res = runner.invoke(outstanding_app, [])
    assert res.exit_code == 0, res.output
    assert f"Showing {RENDER_CAP} of 10" in res.output, res.output


# --- 2.5 the SessionStart block ---------------------------------------------

def test_hook_block_is_silent_on_zero(root: Path):
    result = runner.invoke(outstanding_app, [])
    assert result.exit_code == 0
    assert result.stdout.strip() == ""


def test_hook_block_positive_control(root: Path):
    """The pair for the test above.

    An empty-output assertion alone cannot distinguish a genuinely clean state
    from a block that is permanently broken and renders nothing either way.
    This is the control that makes the silence meaningful.
    """
    assert runner.invoke(outstanding_app, []).stdout.strip() == ""
    _write_carveouts(root, [_carveout("cv-1", "oos-bug")])
    assert runner.invoke(outstanding_app, []).stdout.strip() != ""


# --- 3.3 output modes --------------------------------------------------------

def test_json_mode_emits_one_object_carrying_both_legs(root: Path):
    _write_carveouts(root, [_carveout("cv-1", "deferred")])
    runner.invoke(outstanding_app, ["ask", "a question"])
    payload = json.loads(runner.invoke(outstanding_app, ["--json"]).stdout)
    assert payload["carveouts"]["total"] == 1
    assert payload["carveouts"]["by_kind"]["deferred"] == 1
    assert len(payload["questions"]) == 1


# --- 5.2 the asking session's own questions lead -----------------------------

def test_own_first(root: Path, monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-mine")
    runner.invoke(outstanding_app, ["ask", "MINE: do we ship the fold arm?"])
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-other")
    runner.invoke(outstanding_app, ["ask", "THEIRS: which base branch?"])

    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-mine")
    out = runner.invoke(outstanding_app, []).stdout
    assert "MINE:" in out
    assert "THEIRS:" in out
    assert out.index("MINE:") < out.index("THEIRS:")
    # Labelled as this session's, so the agent that asked is the one prompted.
    assert "this session" in out.lower()


def test_a_worker_with_no_questions_of_its_own_stays_short(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    """The hook fires in every session, so a bg worker's share must stay small.

    The cap is what keeps it small, not a presence branch. An earlier version
    suppressed every row for a worker and produced two defects: it keyed on
    "is a human here" rather than "is this a worker", so a gemini or agy
    OPERATOR got the truncated form, and it printed "clear <id>" right after
    hiding every id. Three rows plus a count is already short.
    """
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-other")
    for i in range(6):
        runner.invoke(outstanding_app, ["ask", f"question number {i}"])

    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-quiet")
    monkeypatch.setenv("FNO_AGENT_SELF", "worker-quiet")
    monkeypatch.delenv("FNO_THINK_SPAWN_PRESENCE", raising=False)
    out = runner.invoke(outstanding_app, []).stdout
    assert "6 open question" in out
    # Bounded by the cap, never the whole queue.
    assert out.count("question number") == 3
    assert "3 more" in out
    # Every rendered row carries an id, so the clear instruction has an operand.
    assert "fno outstanding clear" in out
    assert out.count("q-") >= 3


def test_an_attended_session_does_see_other_sessions_questions(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    """The pair for the test above, and the reason ownership cannot be the gate.

    The operator owns none of these questions - workers asked them - so gating
    the render on ownership would show the one person who can answer exactly
    nothing. Attendedness is the discriminator, not ownership.
    """
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-other")
    for i in range(6):
        runner.invoke(outstanding_app, ["ask", f"question number {i}"])

    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-operator")
    monkeypatch.delenv("FNO_AGENT_SELF", raising=False)
    monkeypatch.delenv("FNO_BG", raising=False)
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    out = runner.invoke(outstanding_app, []).stdout
    assert "6 open question" in out
    # Capped: a growing pile reads as a number, not as a wall.
    assert out.count("question number") == 3
    assert "3 more" in out


def test_render_caps_rows_and_states_the_drop_count(root: Path):
    for i in range(5):
        runner.invoke(outstanding_app, ["ask", f"q{i}"])
    out = runner.invoke(outstanding_app, []).stdout
    assert "5 open question" in out
    assert "2 more" in out
