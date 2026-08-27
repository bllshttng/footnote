"""The watchdog's unfinished-work findings become one durable operator question.

Replaces the stale-session question: rows no verb can clear are noise, and
the durable ask now names the finding identities and the one command that
clears each. Dedup keys on outcome identity, not session rows.
"""
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


def _finding(node: str = "x-7d02", *, basis: str = "in_progress, claim free, idle 116h"):
    from fno.agents import unfinished_work as uw

    return uw.Finding(
        kind=uw.KIND_STARTED,
        subject=node,
        basis=basis,
        clear_command=f"/fno:target {node}",
        node_id=node,
        age_s=116 * 3600,
    )


def _run(root: Path, findings):
    return _subject().escalate_unfinished(
        findings,
        root=root,
        session_id="watchdog-test",
        cwd=root,
    )


def test_same_finding_set_records_exactly_one_question(tmp_path: Path) -> None:
    findings = [_finding("x-a"), _finding("x-b")]

    first_outcome, first_id = _run(tmp_path, findings)
    second_outcome, second_id = _run(tmp_path, list(reversed(findings)))

    assert first_outcome == "recorded"
    assert second_outcome == "duplicate"
    assert second_id == first_id
    assert len(read_open_questions(tmp_path)) == 1


def test_question_names_the_clearing_verbs_not_session_rows() -> None:
    subject = _subject()
    findings = [_finding("x-7d02"), _finding("x-3b05")]
    key = subject.dedupe_key(
        [f"{f.kind}:{f.subject}" for f in findings]
    )
    text = subject.question_text(findings, key)

    assert f"[{subject.MARKER}:{key}]" in text
    assert "unfinished-work finding(s)" in text
    assert "/fno:target x-7d02" in text
    assert "/fno:target x-3b05" in text
    # The retired session-bookkeeping phrasing must not survive the rewrite.
    assert "stale row" not in text
    assert "--only stale" not in text
    assert "reap it or resume it" not in text


def test_identity_change_reasks() -> None:
    subject = _subject()
    first = [_finding("x-a")]
    second = [_finding("x-b")]

    key_one = subject.dedupe_key([f"{f.kind}:{f.subject}" for f in first])
    key_two = subject.dedupe_key([f"{f.kind}:{f.subject}" for f in second])
    assert key_one != key_two


def test_large_finding_set_keeps_marker_count_and_cap(tmp_path: Path) -> None:
    from fno.events import QUESTION_CAP

    findings = [_finding(f"x-{i:04d}") for i in range(150)]

    first_outcome, first_id = _run(tmp_path, findings)
    second_outcome, _second_id = _run(tmp_path, list(reversed(findings)))

    (question,) = read_open_questions(tmp_path)
    assert len(question.question) <= QUESTION_CAP
    assert "150 unfinished-work finding(s)" in question.question
    assert first_outcome == "recorded"
    assert second_outcome == "duplicate"
    assert first_id


def test_unreadable_store_raises_instead_of_recording_again(tmp_path: Path) -> None:
    from fno.outstanding.core import OutstandingError, events_path

    path = events_path(tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.mkdir()

    with pytest.raises(OutstandingError):
        _run(tmp_path, [_finding("x-a")])


def test_empty_finding_set_is_a_named_noop(tmp_path: Path) -> None:
    assert _run(tmp_path, []) == ("none", "")
    assert read_open_questions(tmp_path) == []
