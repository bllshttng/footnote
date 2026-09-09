"""The stale-row lane (x-c186): a row past the 12h wake ceiling lands in the
durable question channel, deduped on outcome identity, and the channel is
reconciled - never piled up - as the measured set changes.

Classification runs through the real ``run_sweep`` with only the
fleet-enumeration seams injected; the fold under test is ``reconcile_stale``.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.outstanding.core import read_open_questions, read_question_events

_NOW = 1_800_000_000.0


@pytest.fixture(autouse=True)
def isolate_question_index(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "fno.paths.questions_jsonl",
        lambda: tmp_path / "questions.jsonl",
        raising=False,
    )


def _tail(text: str, age_s: float):
    from fno.agents.watchdog import TailFacts

    return TailFacts(
        [(_NOW - age_s, text)], _NOW - age_s, text, "assistant", text
    )


def _stale_row(sid: str = "dddd4444-0000", name: str = "k1"):
    from fno.agents.watchdog import Row

    return Row(sid, name, "stopped", None, f"/tmp/{name}")


def _stale_run(root: Path, rows, transcripts):
    """The verb's flow with only the fleet-enumeration seams injected:
    classification, filtering and the fold all run for real."""
    from fno.agents import stale_lane as se
    from fno.agents import watchdog as wd

    payload, out_rows = wd.run_sweep(
        now_s=_NOW,
        rows_provider=lambda: (rows, []),
        transcript_fn=lambda sid: transcripts.get(sid),
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
    )
    assert not payload.get("refused")
    stale_pairs = [
        (wd.Verdict(**data), row)
        for data, row in zip(payload["verdicts"], out_rows)
        if data["verdict"] == wd.STALE
    ]
    return se.reconcile_stale(
        stale_pairs, root=root, session_id="watchdog-test", cwd=root
    )


def test_stale_row_asks_and_names_the_row(tmp_path: Path) -> None:
    outcome, qid = _stale_run(
        tmp_path,
        [_stale_row()],
        {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440)},
    )
    assert outcome == "asked"
    [question] = read_open_questions(tmp_path)
    assert question.id == qid
    assert "[watchdog-stale:" in question.question
    assert "k1" in question.question
    assert "wake ceiling" in question.question
    assert "fno agents watchdog --only stale" in question.ask


def test_row_under_the_ceiling_produces_no_item(tmp_path: Path) -> None:
    outcome, _ = _stale_run(
        tmp_path, [_stale_row()], {"dddd4444-0000": _tail("stopped mid turn", 3600)}
    )
    assert outcome == "none"
    assert read_open_questions(tmp_path) == []


def test_unchanged_set_is_a_duplicate_not_a_second_ask(tmp_path: Path) -> None:
    transcripts = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440)}
    first_outcome, first_id = _stale_run(tmp_path, [_stale_row()], transcripts)
    second_outcome, second_id = _stale_run(tmp_path, [_stale_row()], transcripts)

    assert first_outcome == "asked"
    assert second_outcome == "duplicate"
    assert second_id == first_id
    assert len(read_open_questions(tmp_path)) == 1


def test_changed_set_closes_the_old_ask_and_asks_fresh(tmp_path: Path) -> None:
    one = _stale_row("dddd4444-0000", "k1")
    transcripts = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440)}
    _first_outcome, first_id = _stale_run(tmp_path, [one], transcripts)

    two = _stale_row("eeee5555-0000", "k2")
    transcripts2 = {
        "dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60),
        "eeee5555-0000": _tail("blocked mid turn", 30 * 1440 * 60),
    }
    outcome, new_id = _stale_run(tmp_path, [one, two], transcripts2)

    assert outcome == "asked"
    assert new_id != first_id
    open_qs = read_open_questions(tmp_path)
    assert len(open_qs) == 1
    assert open_qs[0].id == new_id
    assert "k2" in open_qs[0].question


def test_emptied_set_closes_the_open_ask(tmp_path: Path) -> None:
    transcripts = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60)}
    _outcome, asked_id = _stale_run(tmp_path, [_stale_row()], transcripts)

    outcome, closed_id = _stale_run(tmp_path, [_stale_row()], {})

    assert outcome == "closed"
    assert closed_id == asked_id
    assert read_open_questions(tmp_path) == []


def _answer(qid: str, answer: str, root: Path, *, closed_by: str = "operator") -> None:
    from fno.events import operator_question_closed
    from fno.outstanding.core import append_question_event

    append_question_event(
        operator_question_closed(
            question_id=qid,
            answer=answer,
            closed_by=closed_by,
            source="daemon",
        ),
        root,
    )


def test_answered_ask_suppresses_the_renag(tmp_path: Path) -> None:
    """An answered ask is a consumed ask: the next sweep with an UNCHANGED
    row set reports 'answered' (the sweep RAN - a positive marker, never an
    absence-only no-mail claim) and appends no new question row."""
    transcripts = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60)}
    _outcome, asked_id = _stale_run(tmp_path, [_stale_row()], transcripts)
    assert read_open_questions(tmp_path)

    _answer(asked_id, "noted; leave them", tmp_path)

    outcome, seen_id = _stale_run(tmp_path, [_stale_row()], transcripts)

    assert outcome == "answered"
    assert seen_id == asked_id
    assert read_open_questions(tmp_path) == []


def test_mechanical_supersede_close_does_not_suppress_a_returning_set(
    tmp_path: Path,
) -> None:
    """A close minted by the fold itself is not an answer: when the set
    changes away and comes back, the returning set must re-ask."""
    one = _stale_row("dddd4444-0000", "k1")
    transcripts_one = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60)}
    _outcome, first_id = _stale_run(tmp_path, [one], transcripts_one)

    two = _stale_row("eeee5555-0000", "k2")
    transcripts_two = {
        "dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60),
        "eeee5555-0000": _tail("blocked mid turn", 30 * 1440 * 60),
    }
    _outcome, second_id = _stale_run(tmp_path, [one, two], transcripts_two)
    assert second_id != first_id
    _answer(second_id, "k2 reaped", tmp_path)

    outcome, third_id = _stale_run(tmp_path, [one], transcripts_one)

    assert outcome == "asked"
    assert third_id not in (first_id, second_id)


def test_answered_finding_ask_suppresses_the_renag(tmp_path: Path) -> None:
    """The unfinished-work emitter rides the same fold: an answered finding
    set never re-asks while unchanged."""
    from types import SimpleNamespace

    from fno.agents.stale_escalate import escalate_unfinished

    finding = SimpleNamespace(
        kind="dirty", subject="/w/x", basis="82 files dirty",
        clear_command="fno agents workspace worktree cleanup", node_id=None,
        pr_number=None, cwd="/w/x", age_s=100.0,
    )
    first_outcome, asked_id = escalate_unfinished(
        [finding], root=tmp_path, session_id="watchdog-test", cwd=tmp_path
    )
    assert first_outcome == "recorded"

    _answer(asked_id, "cleaning it now", tmp_path)

    outcome, seen_id = escalate_unfinished(
        [finding], root=tmp_path, session_id="watchdog-test", cwd=tmp_path
    )

    assert outcome == "answered"
    assert seen_id == asked_id
    assert read_open_questions(tmp_path) == []


def test_answered_ask_asks_again_after_the_set_empties(tmp_path: Path) -> None:
    """The suppression is per-episode: an answered row that disappears and
    later recurs is NEW work, and must ask again - the measured case of a
    path answered once, cleaned, and re-dirtied weeks later."""
    from fno.agents.stale_escalate import answered_question

    transcripts = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60)}
    _outcome, asked_id = _stale_run(tmp_path, [_stale_row()], transcripts)
    _answer(asked_id, "reaped", tmp_path)
    assert answered_question(
        tmp_path, _key_of(tmp_path, asked_id), marker="watchdog-stale"
    ) == asked_id

    _stale_run(tmp_path, [_stale_row()], {})  # the set empties: episode ends

    outcome, fresh_id = _stale_run(tmp_path, [_stale_row()], transcripts)

    assert outcome == "asked"
    assert fresh_id != asked_id
    assert answered_question(
        tmp_path, _key_of(tmp_path, asked_id), marker="watchdog-stale"
    ) is None


def _key_of(root: Path, qid: str) -> str:
    """The dedupe key inside a question's marker tag (test helper)."""
    for q in read_open_questions(root):
        if q.id == qid:
            return q.question.split("[watchdog-stale:", 1)[1].split("]", 1)[0]
    for rec in read_question_events():
        data = rec.get("data") or {}
        text = str(data.get("question") or "")
        if data.get("question_id") == qid and "[watchdog-stale:" in text:
            return text.split("[watchdog-stale:", 1)[1].split("]", 1)[0]
    raise AssertionError(f"no question {qid} found")


def test_answer_stands_when_the_set_changes_without_emptying(
    tmp_path: Path,
) -> None:
    """The only episode boundary is the empty set: a set that changed away
    and back without ever emptying is still inside the answered episode, so
    the answer holds and the fold does not re-nag."""
    one = _stale_row("dddd4444-0000", "k1")
    transcripts_one = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60)}
    _outcome, first_id = _stale_run(tmp_path, [one], transcripts_one)
    _answer(first_id, "noted", tmp_path)

    two = _stale_row("eeee5555-0000", "k2")
    transcripts_two = {
        "dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60),
        "eeee5555-0000": _tail("blocked mid turn", 30 * 1440 * 60),
    }
    _outcome, _second_id = _stale_run(tmp_path, [one, two], transcripts_two)

    outcome, seen_id = _stale_run(tmp_path, [one], transcripts_one)

    assert outcome == "answered"
    assert seen_id == first_id


def test_answered_visit_closes_stragglers(tmp_path: Path) -> None:
    """The answered branch reconciles like the duplicate branch: an open
    straggler from an interrupted supersede cannot outlive the visit."""
    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    transcripts = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60)}
    _outcome, asked_id = _stale_run(tmp_path, [_stale_row()], transcripts)
    _answer(asked_id, "noted", tmp_path)

    straggler_id = "q-feedface"
    append_question_event(
        operator_question(
            question_id=straggler_id,
            question="[watchdog-stale:000000000000] straggler: k0",
            session_id="watchdog-test",
            cwd=str(tmp_path),
            ask="triage",
            source="daemon",
        ),
        tmp_path,
    )
    assert len(read_open_questions(tmp_path)) == 1

    outcome, seen_id = _stale_run(tmp_path, [_stale_row()], transcripts)

    assert outcome == "answered"
    assert seen_id == asked_id
    assert read_open_questions(tmp_path) == []


def test_answered_finding_asks_again_after_the_finding_disappears(
    tmp_path: Path,
) -> None:
    """The unfinished-work emitter resets the same way: answered dirty:/w/x,
    cleaned, re-dirtied later asks again instead of staying silent forever."""
    from types import SimpleNamespace

    from fno.agents.stale_escalate import escalate_unfinished

    finding = SimpleNamespace(
        kind="dirty", subject="/w/x", basis="82 files dirty",
        clear_command="fno agents workspace worktree cleanup", node_id=None,
        pr_number=None, cwd="/w/x", age_s=100.0,
    )
    _outcome, asked_id = escalate_unfinished(
        [finding], root=tmp_path, session_id="watchdog-test", cwd=tmp_path
    )
    _answer(asked_id, "cleaning", tmp_path)

    outcome, _qid = escalate_unfinished(
        [], root=tmp_path, session_id="watchdog-test", cwd=tmp_path
    )
    assert outcome == "none"  # the empty visit is the episode boundary

    outcome, fresh_id = escalate_unfinished(
        [finding], root=tmp_path, session_id="watchdog-test", cwd=tmp_path
    )

    assert outcome == "recorded"
    assert fresh_id != asked_id


def test_reset_writes_nothing_for_a_clean_fleet(tmp_path: Path) -> None:
    """The reset row is lazy: an empty visit with no suppressible answer
    appends no journal row, so a quiet fleet does not grow the journal."""
    import json

    from fno.outstanding.core import events_path

    _stale_run(tmp_path, [_stale_row()], {})  # empty from the start

    path = events_path(tmp_path)
    if not path.exists():
        return  # nothing was ever written: vacuously clean
    resets = [
        json.loads(line)
        for line in path.read_text(encoding="utf-8").splitlines()
        if json.loads(line).get("type") == "operator_decision"
        and (json.loads(line).get("data") or {}).get("subject") == "watchdog-stale:reset"
    ]
    assert resets == []


def test_duplicate_run_closes_straggler_asks_from_an_interrupted_supersede(
    tmp_path: Path,
) -> None:
    """The append-before-close ordering costs a failed close a duplicate
    outcome, never an empty channel - and the duplicate visit finishes the
    closes the interrupted run left behind."""
    transcripts = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60)}
    _outcome, kept_id = _stale_run(tmp_path, [_stale_row()], transcripts)

    # Simulate the interrupted supersede: an older-key ask still open beside
    # the current one.
    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    stale_id = "q-deadbeef"
    append_question_event(
        operator_question(
            question_id=stale_id,
            question=f"[watchdog-stale:000000000000] superseded remain: k0",
            session_id="watchdog-test",
            cwd=str(tmp_path),
            ask="triage",
            source="daemon",
        ),
        tmp_path,
    )
    assert len(read_open_questions(tmp_path)) == 2

    outcome, seen_id = _stale_run(tmp_path, [_stale_row()], transcripts)

    assert outcome == "duplicate"
    assert seen_id == kept_id
    assert [q.id for q in read_open_questions(tmp_path)] == [kept_id]


def test_every_closed_ask_carries_a_decision_record(tmp_path: Path) -> None:
    """Positive marker, not an absence: the stop gate holds a session whose
    question was closed with an answer but left no operator_decision record,
    and `fno backlog decide` refuses agent sessions - so the fold must write
    its own record keyed on the question id, at agent authority."""
    import json

    from fno.outstanding.core import events_path

    transcripts = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60)}
    _outcome, asked_id = _stale_run(tmp_path, [_stale_row()], transcripts)
    outcome, _closed = _stale_run(tmp_path, [_stale_row()], {})

    assert outcome == "closed"
    records = []
    for line in events_path(tmp_path).read_text(encoding="utf-8").splitlines():
        event = json.loads(line)
        if event.get("type") == "operator_decision":
            records.append(event["data"])
    mine = [r for r in records if r.get("question_id") == asked_id]
    assert len(mine) == 1
    assert mine[0]["authority_source"] == "agent"
    assert mine[0]["decided_by"] == "fno agents stale-escalate"


def test_refused_sweep_escalates_and_closes_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A sweep that measured nobody must not speak for the channel: no ask
    and no close - an empty read is a measurement failure, never evidence."""
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    def refused(**_kwargs):
        payload = {
            "refused": "roster unavailable",
            "verdicts": [],
            "counts": {},
            "warnings": [],
        }
        return payload, []

    monkeypatch.setattr("fno.agents.watchdog.run_sweep", refused)
    monkeypatch.setattr("fno.carveout.core.resolve_carveout_root", lambda: tmp_path)
    result = CliRunner().invoke(agents_app, ["stale-escalate", "--json"])
    assert result.exit_code == 0
    assert '"outcome": "refused"' in result.output
    assert read_open_questions(tmp_path) == []


def test_full_verb_asked_path_end_to_end(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The real CLI verb with only the fleet-enumeration seams injected:
    classification, filtering, the fold, and the --json glue all run, so a
    glue-only regression (an attribute drift, a mis-keyed field) cannot ship
    green off function-level tests alone."""
    from typer.testing import CliRunner

    from fno.agents import watchdog as wd
    from fno.agents.cli import agents_app

    real_run_sweep = wd.run_sweep

    def seeded(**_kwargs):
        return real_run_sweep(
            now_s=_NOW,
            rows_provider=lambda: ([_stale_row()], []),
            transcript_fn=lambda sid: {
                "dddd4444-0000": _tail("stopped mid turn", 61 * 1440 * 60)
            }.get(sid),
            claim_fn=lambda node: {},
            graph_fn=lambda: {},
        )

    monkeypatch.setattr("fno.agents.watchdog.run_sweep", seeded)
    monkeypatch.setattr("fno.carveout.core.resolve_carveout_root", lambda: tmp_path)
    monkeypatch.setattr(
        "fno.carveout.core.resolve_session_id", lambda _root: "watchdog-test"
    )
    result = CliRunner().invoke(agents_app, ["stale-escalate", "--json"])
    assert result.exit_code == 0, result.output
    assert '"outcome": "asked"' in result.output
    assert '"stale_count": 1' in result.output
    assert '"oldest_h": 1464' in result.output
    assert "Summary: 1 stale, outcome asked, oldest 1464h" in result.output
    [question] = read_open_questions(tmp_path)
    assert "k1" in question.question
