"""The stale-row lane (x-c186): a row past the 12h wake ceiling lands in the
durable question channel, deduped on outcome identity, and the channel is
reconciled - never piled up - as the measured set changes.

Classification runs through the real ``run_sweep`` with only the
fleet-enumeration seams injected; the fold under test is ``reconcile_stale``.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.outstanding.core import read_open_questions

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
        [(_NOW - age_s, text)], _NOW - age_s, text, "assistant", text, "text"
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
    transcripts = {"dddd4444-0000": _tail("stopped mid turn", 61 * 1440)}
    _outcome, asked_id = _stale_run(tmp_path, [_stale_row()], transcripts)

    outcome, closed_id = _stale_run(tmp_path, [_stale_row()], {})

    assert outcome == "closed"
    assert closed_id == asked_id
    assert read_open_questions(tmp_path) == []


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
