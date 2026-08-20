"""Plan verification 7, second half: one stalled board is ONE question.

The first half - that both king terminals reach the verb, over the same stalled
set - is `both_king_terminals_escalate_over_the_same_stalled_set` in
`crates/fno-agents/tests/loop_check.rs`. It drives the two real paths against a
mock `fno` and asserts the `--stalled` argument they produce. This file takes
that argument and asserts what the verb does with it twice.

Neither half is the verification alone. The seam between them is the `--stalled`
string: the Rust test pins that both arms emit it identically, this one pins
that an identical string never records a second question.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.king.escalate import dedupe_key, escalate, question_text
from fno.outstanding.core import read_open_questions

STALLED = ["undispatched:x-1234", "undispatched:x-5678"]


@pytest.fixture(autouse=True)
def isolate_question_index(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "fno.paths.questions_jsonl",
        lambda: tmp_path / "questions.jsonl",
        raising=False,
    )


def _run(root: Path, ids: list[str], reason: str = "NoProgress") -> tuple[str, str]:
    return escalate(ids, reason=reason, root=root, session_id="k-test", cwd=root)


def test_one_stalled_board_records_exactly_one_question(tmp_path: Path) -> None:
    """Both terminals escalating the same set leaves one question, not two.

    This is the whole point of the verb existing instead of a bare
    `fno outstanding ask`: the walk arm parks and the stop hook terminates over
    the same stalled board, and an operator handed two identical questions stops
    reading the queue this feature depends on.
    """
    first_outcome, first_id = _run(tmp_path, STALLED)
    second_outcome, second_id = _run(tmp_path, list(reversed(STALLED)))

    assert first_outcome == "recorded"
    assert second_outcome == "duplicate"
    assert second_id == first_id
    assert len(read_open_questions(tmp_path)) == 1


def test_a_different_stalled_set_is_a_different_question(tmp_path: Path) -> None:
    """The board changed, so the ask changed. Dedupe must not swallow that."""
    _run(tmp_path, STALLED)
    outcome, _ = _run(tmp_path, ["undispatched:x-9999"])

    assert outcome == "recorded"
    assert len(read_open_questions(tmp_path)) == 2


def test_the_key_ignores_order_and_repeats(tmp_path: Path) -> None:
    assert dedupe_key(["b", "a"]) == dedupe_key(["a", "b", "a"])
    assert dedupe_key(["a"]) != dedupe_key(["a", "b"])


def test_an_empty_stalled_set_never_reads_as_a_clean_board(tmp_path: Path) -> None:
    """An unreadable board escalates with no ids. The text must say so.

    Absence has two explanations. A question that named zero rows and stopped
    there would be indistinguishable from a board with nothing on it, which is
    the one state that must never produce a question at all.
    """
    _, qid = _run(tmp_path, [], reason="NoProgress")
    (question,) = read_open_questions(tmp_path)

    assert question.id == qid
    assert "could not read" in question.question
    assert "clean" not in question.question


def test_the_question_names_the_rows_and_carries_the_key() -> None:
    key = dedupe_key(STALLED)
    text = question_text(STALLED, key, "NoProgress")

    assert "undispatched:x-1234" in text
    assert "undispatched:x-5678" in text
    assert f"[king-escalation:{key}]" in text
    # The operator's actual decision hinges on this: the king is GONE.
    assert "exited" in text


def test_an_unreadable_store_is_not_an_empty_one(tmp_path: Path) -> None:
    """A read failure must raise, never look like "nothing asked yet".

    A reader that could not tell those apart would file a fresh question on
    every fire, which is the pathology the dedupe exists to prevent - arriving
    through the error path instead of the happy one.
    """
    from fno.outstanding.core import OutstandingError, events_path

    path = events_path(tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.mkdir()  # a directory where the journal should be: unreadable, not absent

    with pytest.raises(OutstandingError):
        _run(tmp_path, STALLED)


def test_a_huge_stalled_board_still_dedupes(tmp_path: Path) -> None:
    """The marker must survive `operator_question`'s truncation.

    The defect: the marker sat at the END, after the full comma-joined id list.
    `operator_question` caps the recorded text at QUESTION_CAP, so a board with
    enough stalled rows pushed the marker off the end. `already_asked` then
    matched nothing and every respawned king filed a duplicate - precisely the
    failure this module claims to prevent, reached through its own happy path.

    Six queues at the board's 25-row cap is ~150 rows, so this size is
    reachable, not hypothetical.
    """
    from fno.events import QUESTION_CAP

    huge = [f"undispatched:x-{i:04d}" for i in range(150)]

    first_outcome, first_id = _run(tmp_path, huge)
    second_outcome, second_id = _run(tmp_path, list(reversed(huge)))

    (question,) = read_open_questions(tmp_path)
    assert len(question.question) <= QUESTION_CAP
    assert f"[king-escalation:{dedupe_key(huge)}]" in question.question, (
        "the marker must survive truncation, so it leads the text"
    )
    assert first_outcome == "recorded"
    assert second_outcome == "duplicate"
    assert second_id == first_id
    # The count is load-bearing even when the rows are elided.
    assert "150 board row(s)" in question.question
