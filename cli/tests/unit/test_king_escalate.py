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


def test_escalations_carry_the_king_session_as_asker(tmp_path: Path) -> None:
    """BREAK 1: an answered escalation must be deliverable to a successor king.

    The king that asked is dead when the operator answers, but the durable mail
    tier reaches the respawned one; neither works without an address on the row.
    """
    from fno.harness_identity import canonical_handle

    _, _qid = _run(tmp_path, STALLED)
    (question,) = read_open_questions(tmp_path)

    assert question.asker == canonical_handle("k-test")


def test_a_sessionless_escalation_records_without_an_asker(tmp_path: Path) -> None:
    """No session id is the legacy shape; the question still lands, asker None."""
    escalate(STALLED, reason="NoProgress", root=tmp_path, session_id=None, cwd=tmp_path)
    (question,) = read_open_questions(tmp_path)

    assert question.asker is None


# ---------------------------------------------------------------------------
# AC4-HP (x-3ecf): king escalate resolves the presiding crown first
# ---------------------------------------------------------------------------


def _entry(name: str, **kw):
    from fno.agents.registry import AgentEntry

    harness = kw.pop("harness", "claude")
    kw.setdefault("cwd", "/w")
    kw.setdefault("harness_session_id", f"{name}-session")
    return AgentEntry(name=name, log_path="", harness=harness, **kw)


def _prepare_court(monkeypatch, tmp_path: Path, rows) -> None:
    import json

    from fno import paths
    from fno.agents.registry import write_registry
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    write_registry(rows)
    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(
        json.dumps({"entries": [{"id": "x-epic", "type": "epic", "project": "fno", "status": "ready"}]}),
        encoding="utf-8",
    )


def test_ac4_hp_a_live_l1_crown_presides_over_its_epic_set(tmp_path: Path, monkeypatch) -> None:
    """An L2 crown over a scope an L1 crown's project contains: the L1
    king's own entry comes back, named."""
    from fno.king.escalate import resolve_presiding_king

    _prepare_court(
        monkeypatch,
        tmp_path,
        [
            _entry("l2-king", status="busy", crown_level=2, crown_scope="x-epic"),
            _entry("l1-king", status="busy", crown_level=1, crown_scope="fno"),
        ],
    )
    presiding = resolve_presiding_king("l2-king-session")
    assert presiding is not None
    assert presiding["holder"] == "l1-king"


def test_ac4_hp_no_higher_crown_reads_as_none(tmp_path: Path, monkeypatch) -> None:
    """The converse: an L1 crown (already the top rung reachable here) has
    nothing to escalate to, so the caller falls through to the operator."""
    from fno.king.escalate import resolve_presiding_king

    _prepare_court(
        monkeypatch, tmp_path, [_entry("l1-king", status="busy", crown_level=1, crown_scope="fno")]
    )
    assert resolve_presiding_king("l1-king-session") is None


def test_ac4_hp_an_unknown_session_falls_through_quietly(tmp_path: Path, monkeypatch) -> None:
    from fno.king.escalate import resolve_presiding_king

    _prepare_court(monkeypatch, tmp_path, [])
    assert resolve_presiding_king("no-such-session") is None


def test_ac4_hp_a_uuid_session_id_matches_case_insensitively(tmp_path: Path, monkeypatch) -> None:
    """A UUID-family id differing only in case is still the caller's own row
    (harness_identity.session_identity_key's own contract) - a raw string
    comparison here would silently read every such call as uncrowned."""
    from fno.king.escalate import resolve_presiding_king

    stored = "aaaa1111-bbbb-4ccc-8ddd-eeeeeeeeeeee"
    _prepare_court(
        monkeypatch,
        tmp_path,
        [
            _entry(
                "l2-king",
                status="busy",
                crown_level=2,
                crown_scope="x-epic",
                harness_session_id=stored,
            ),
            _entry("l1-king", status="busy", crown_level=1, crown_scope="fno"),
        ],
    )
    presiding = resolve_presiding_king(stored.upper())
    assert presiding is not None
    assert presiding["holder"] == "l1-king"
    assert resolve_presiding_king(None) is None


def test_mail_presiding_king_is_false_with_no_fno_on_path(monkeypatch) -> None:
    from fno.king.escalate import mail_presiding_king

    monkeypatch.setattr("shutil.which", lambda _name: None)
    assert mail_presiding_king("l1-king", STALLED, "NoProgress") is False


def test_mail_presiding_king_true_only_on_a_zero_exit(monkeypatch) -> None:
    from fno.king.escalate import mail_presiding_king

    class _Proc:
        def __init__(self, code: int) -> None:
            self.returncode = code

    monkeypatch.setattr("shutil.which", lambda _name: "/usr/bin/fno")
    monkeypatch.setattr("subprocess.run", lambda *a, **k: _Proc(0))
    assert mail_presiding_king("l1-king", STALLED, "NoProgress") is True

    monkeypatch.setattr("subprocess.run", lambda *a, **k: _Proc(1))
    assert mail_presiding_king("l1-king", STALLED, "NoProgress") is False
