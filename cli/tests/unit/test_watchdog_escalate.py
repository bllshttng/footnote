"""The watchdog's stale bucket becomes one durable operator question."""
from __future__ import annotations

import importlib
from pathlib import Path

import pytest

from fno.outstanding.core import read_open_questions


@pytest.fixture(autouse=True)
def isolate_question_index(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "fno.paths.questions_jsonl",
        lambda: tmp_path / "questions.jsonl",
        raising=False,
    )


def _subject():
    try:
        return importlib.import_module("fno.agents.stale_escalate")
    except ModuleNotFoundError:
        pytest.fail("fno.agents.stale_escalate is missing")


def _row(row_id: str, *, node: str | None = "x-1234", basis: str = "blocked 12h"):
    return _subject().StaleRow(
        row_id=row_id,
        name=f"worker-{row_id}",
        state="blocked",
        node=node,
        basis=basis,
    )


def _run(root: Path, rows):
    return _subject().escalate_stale(
        rows,
        root=root,
        session_id="watchdog-test",
        cwd=root,
    )


def test_same_stale_set_records_exactly_one_question(tmp_path: Path) -> None:
    rows = [_row("b", basis="blocked 12h"), _row("a", basis="blocked 7h")]

    first_outcome, first_id = _run(tmp_path, rows)
    second_outcome, second_id = _run(tmp_path, list(reversed(rows)))

    assert first_outcome == "recorded"
    assert second_outcome == "duplicate"
    assert second_id == first_id
    assert len(read_open_questions(tmp_path)) == 1


def test_unknown_provenance_is_explicit() -> None:
    subject = _subject()
    row = _row("a", node=None, basis="")

    text = subject.question_text([row], subject.dedupe_key([row.row_id]))

    assert "node unknown" in text
    assert "no basis recorded" in text
    assert "x-" not in text


def test_large_stale_set_keeps_marker_count_and_dedupe(tmp_path: Path) -> None:
    from fno.events import QUESTION_CAP

    rows = [_row(f"row-{i:03d}", basis=f"blocked {i}h") for i in range(150)]

    first_outcome, first_id = _run(tmp_path, rows)
    second_outcome, second_id = _run(tmp_path, list(reversed(rows)))

    (question,) = read_open_questions(tmp_path)
    assert len(question.question) <= QUESTION_CAP
    assert f"[watchdog-stale:{_subject().dedupe_key([r.row_id for r in rows])}]" in question.question
    assert "150 stale row(s)" in question.question
    assert first_outcome == "recorded"
    assert second_outcome == "duplicate"
    assert second_id == first_id


def test_unreadable_store_raises_instead_of_recording_again(tmp_path: Path) -> None:
    from fno.outstanding.core import OutstandingError, events_path

    path = events_path(tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.mkdir()

    with pytest.raises(OutstandingError):
        _run(tmp_path, [_row("a")])


def test_empty_stale_bucket_is_a_named_noop(tmp_path: Path) -> None:
    assert _run(tmp_path, []) == ("none", "")
    assert read_open_questions(tmp_path) == []
