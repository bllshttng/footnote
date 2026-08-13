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
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.outstanding.cli import outstanding_app

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
    _write_carveouts(root, [_carveout("cv-old", "oos-bug", ts="2026-01-01T00:00:00Z")])
    result = runner.invoke(outstanding_app, [])
    assert result.exit_code == 0
    assert "2026-01-01" in result.output or "oldest" in result.output.lower()


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


def test_a_malformed_events_line_is_skipped_never_raised(root: Path):
    qid = runner.invoke(outstanding_app, ["ask", "still readable?"]).stdout.strip().splitlines()[-1]
    events = root / ".fno" / "events.jsonl"
    with events.open("a", encoding="utf-8") as fh:
        fh.write("{not json at all\n")

    result = runner.invoke(outstanding_app, ["--json"])
    assert result.exit_code == 0, result.output
    assert [q["id"] for q in json.loads(result.stdout)["questions"]] == [qid]


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
    assert out.index("MINE:") < out.index("THEIRS:") if "THEIRS:" in out else True
    # Labelled as this session's, so a worker can tell its own from the pile.
    assert "this session" in out.lower()


def test_a_worker_with_no_questions_of_its_own_stays_short(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-other")
    for i in range(6):
        runner.invoke(outstanding_app, ["ask", f"question number {i}"])

    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-quiet")
    out = runner.invoke(outstanding_app, []).stdout
    assert "6 open question" in out
    # Capped render: a growing pile is visible as a number, not as a wall.
    assert out.count("question number") <= 3


def test_render_caps_rows_and_states_the_drop_count(root: Path):
    for i in range(5):
        runner.invoke(outstanding_app, ["ask", f"q{i}"])
    out = runner.invoke(outstanding_app, []).stdout
    assert "5 open question" in out
    assert "2 more" in out
