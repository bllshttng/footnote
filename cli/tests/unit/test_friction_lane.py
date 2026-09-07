"""The friction lane: the report-only friction verdicts (contended,
polling_settled) land in ONE reconciled operator question, deduped on
outcome identity, and the channel is reconciled - never piled up - as the
measured set changes.

Classification runs through the real ``run_sweep`` with only the
fleet-enumeration seams injected; the fold under test is
``reconcile_friction``.
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


def _linked_worktree(tmp_path: Path, name: str) -> str:
    wt = tmp_path / name
    wt.mkdir()
    (wt / ".git").write_text("gitdir: /tmp/elsewhere/main\n")
    return str(wt)


def _friction_run(root: Path, rows, transcripts, pr_state_for=None):
    """The verb's flow with only the fleet-enumeration seams injected:
    classification, filtering and the fold all run for real."""
    from fno.agents import friction_lane as fl
    from fno.agents import watchdog as wd

    payload, out_rows = wd.run_sweep(
        now_s=_NOW,
        rows_provider=lambda: (rows, []),
        transcript_fn=lambda sid: transcripts.get(sid),
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
        pr_state_fn=pr_state_for,
    )
    assert not payload.get("refused")
    pairs = [
        (wd.Verdict(**data), row)
        for data, row in zip(payload["verdicts"], out_rows)
        if data["verdict"] in (wd.CONTENDED, wd.POLLING_SETTLED)
    ]
    return fl.reconcile_friction(
        pairs, root=root, session_id="watchdog-test", cwd=root
    )


def _facts(text: str, age_s: float, pr_polls: tuple = ()):
    from fno.agents.watchdog import TailFacts

    return TailFacts(
        [(_NOW - age_s, text)], _NOW - age_s, text, "assistant", text, pr_polls
    )


def test_one_question_names_every_friction_row(tmp_path: Path) -> None:
    wt = _linked_worktree(tmp_path, "w1")
    rows = [
        _row("aaaa1111-0000", "w1", wt),
        _row("bbbb2222-0000", "w2", wt),
        _row("cccc3333-0000", "w3", "/tmp/w3"),
    ]
    transcripts = {
        "aaaa1111-0000": _facts("still on it", 60),
        "bbbb2222-0000": _facts("still on it", 60),
        "cccc3333-0000": _facts(
            "checking", 60,
            (("settled", 42, "MERGED"), ("read", 42, ""), ("read", 42, "")),
        ),
    }
    outcome, qid = _friction_run(
        tmp_path, rows, transcripts, pr_state_for=lambda cwd, n: "MERGED"
    )
    assert outcome == "asked"
    [question] = read_open_questions(tmp_path)
    assert question.id == qid
    assert "[watchdog-friction:" in question.question
    assert "3 contention/polling row(s)" in question.question
    assert "w1" in question.question and "w3" in question.question
    assert "fno agents watchdog --only contended" in question.ask


def _row(sid: str, name: str, cwd: str):
    from fno.agents.watchdog import Row

    return Row(sid, name, "working", None, cwd)


def test_second_run_over_the_same_set_is_a_duplicate(tmp_path: Path) -> None:
    wt = _linked_worktree(tmp_path, "w1")
    rows = [_row("aaaa1111-0000", "w1", wt), _row("bbbb2222-0000", "w2", wt)]
    transcripts = {
        "aaaa1111-0000": _facts("still on it", 60),
        "bbbb2222-0000": _facts("still on it", 60),
    }
    first_outcome, first_id = _friction_run(tmp_path, rows, transcripts)
    second_outcome, second_id = _friction_run(tmp_path, rows, transcripts)
    assert (first_outcome, second_outcome) == ("asked", "duplicate")
    assert second_id == first_id
    assert len(read_open_questions(tmp_path)) == 1


def test_emptied_set_resolves_the_open_question(tmp_path: Path) -> None:
    wt = _linked_worktree(tmp_path, "w1")
    rows = [_row("aaaa1111-0000", "w1", wt), _row("bbbb2222-0000", "w2", wt)]
    transcripts = {
        "aaaa1111-0000": _facts("still on it", 60),
        "bbbb2222-0000": _facts("still on it", 60),
    }
    _outcome, _qid = _friction_run(tmp_path, rows, transcripts)
    assert read_open_questions(tmp_path)
    # The peer goes quiet-and-finished: one live row in the tree, no friction.
    quiet = dict(transcripts)
    quiet["bbbb2222-0000"] = _facts(
        "<promise>PR is green and reviewed</promise>", 1800
    )
    outcome, _closed_id = _friction_run(tmp_path, rows, quiet)
    assert outcome == "closed"
    assert read_open_questions(tmp_path) == []
