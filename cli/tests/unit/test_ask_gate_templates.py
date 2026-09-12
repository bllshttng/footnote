"""Every machine question writer fits the ask gate at its largest shape.

The gate on ``append_question_event`` refuses any ``operator_question`` over
``config.style.word_cap.ask`` masked words (default 40, law d-59af3235), so a
writer template that regresses past the cap loses its signal silently: the
caller catches ``AskRefused`` or the escalation dies. Each test here drives
the REAL writer against a sandbox index at its largest realistic shape and
asserts the recorded text passes ``ask_refusal``.
"""
from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.outstanding.core import AskRefused, ask_refusal, read_open_questions

CAP = 40


@pytest.fixture(autouse=True)
def isolate_question_index(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "fno.paths.questions_jsonl",
        lambda: tmp_path / "questions.jsonl",
        raising=False,
    )


def _assert_fits(question) -> None:
    assert "\n" not in question.question.strip()
    assert (
        ask_refusal(
            question.question,
            node=question.node,
            blocks=question.blocks,
            cap=CAP,
            require_pointer=False,
        )
        is None
    ), question.question


# --- king escalation ---------------------------------------------------------


def test_king_escalation_of_25_stalled_rows_with_unreadable_liveness_fits(
    tmp_path: Path,
) -> None:
    from fno.king.escalate import dedupe_key, escalate

    stalled = [f"stalled_holder:x-{i:06x}" for i in range(1, 26)]
    stalled.append("stalled_holder:x-1005")
    outcome, _qid = escalate(
        stalled,
        reason="NoProgress",
        root=tmp_path,
        session_id="k-fit",
        cwd=tmp_path,
        live=None,
        unknown_reason="reign_state unreadable: reader exploded",
    )
    assert outcome == "recorded"
    (question,) = read_open_questions(tmp_path)
    assert f"[king-escalation:{dedupe_key(stalled)}]" in question.question
    assert "x-1005" in question.blocks
    _assert_fits(question)


def test_king_escalation_carries_the_scope_as_node(tmp_path: Path) -> None:
    """AC7: a reign scope that spells a node id lands on the row's node."""
    from fno.king.escalate import escalate

    escalate(
        ["undispatched:x-1234"],
        reason="NoProgress",
        root=tmp_path,
        session_id="k-scope",
        cwd=tmp_path,
        scope="x-a792",
    )
    (question,) = read_open_questions(tmp_path)
    assert question.node == "x-a792"


def test_a_scope_that_spells_no_node_sets_none(tmp_path: Path) -> None:
    from fno.king.escalate import escalate

    escalate(
        ["undispatched:x-1234"],
        reason="NoProgress",
        root=tmp_path,
        session_id="k-scope2",
        cwd=tmp_path,
        scope="territory-fno",
    )
    (question,) = read_open_questions(tmp_path)
    assert question.node is None
    assert question.blocks == ("x-1234",)


def test_a_regression_past_the_cap_raises_ask_refused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC9: the gate fires at the shared write path, whatever the writer."""
    import fno.king.escalate as esc

    real = esc.question_text

    def bloated(*a, **k):
        return real(*a, **k) + " " + " ".join(["padding"] * 80)

    monkeypatch.setattr(esc, "question_text", bloated)
    with pytest.raises(AskRefused):
        esc.escalate(
            ["undispatched:x-1234"],
            reason="NoProgress",
            root=tmp_path,
            session_id="k-fat",
            cwd=tmp_path,
        )


# --- watchdog unfinished-work ------------------------------------------------


def _finding(i: int):
    return SimpleNamespace(
        kind="stale_session",
        subject=f"worker-{i}",
        basis=f"{i * 10}.0h since last wake",
        clear_command=f"fno agents reap --kill worker-{i}",
        node_id=f"x-{i:06x}" if i % 3 else None,
    )


def test_unfinished_work_of_10_findings_with_incomplete_scan_fits(
    tmp_path: Path,
) -> None:
    from fno.agents.stale_escalate import dedupe_key, escalate_unfinished

    findings = [_finding(i) for i in range(10)]
    outcome, _qid = escalate_unfinished(
        findings,
        root=tmp_path,
        session_id="wd-fit",
        cwd=tmp_path,
        unknown_dimensions=("transcripts",),
    )
    assert outcome == "recorded"
    (question,) = read_open_questions(tmp_path)
    assert f"[watchdog-unfinished-work:{dedupe_key([f'{f.kind}:{f.subject}' for f in findings])}]" in (
        question.question
    )
    assert "incomplete" in question.question
    assert set(question.blocks) == {f"x-{i:06x}" for i in range(10) if i % 3}
    _assert_fits(question)


# --- the reconcile lanes (stale, reap-hold, friction) ------------------------


def _lane_rows(n: int):
    verdict = SimpleNamespace(
        name="STALE", verdict="stale", row_id="row", basis="12.0h quiet"
    )
    return [
        (
            SimpleNamespace(
                name="STALE",
                verdict="stale",
                row_id=f"row-{i}",
                basis=f"{i * 3}.0h quiet",
            ),
            SimpleNamespace(node=f"x-{i:06x}" if i % 2 else None),
        )
        for i in range(1, n + 1)
    ]


def test_stale_lane_of_30_rows_fits(tmp_path: Path) -> None:
    from fno.agents.stale_lane import reconcile_stale

    outcome, _qid = reconcile_stale(
        _lane_rows(30), root=tmp_path, session_id="lane-1", cwd=tmp_path
    )
    assert outcome == "asked"
    (question,) = read_open_questions(tmp_path)
    assert set(question.blocks) == {f"x-{i:06x}" for i in range(1, 31, 2)}
    _assert_fits(question)


def test_reap_hold_lane_of_30_holds_fits(tmp_path: Path) -> None:
    from fno.agents.stale_lane import reconcile_holds

    holds = [
        {"id": f"ses-{i}", "reason": "deployment-window", "detail": "held for triage", "age_s": 3600 * i}
        for i in range(30)
    ]
    outcome, _qid = reconcile_holds(
        holds, root=tmp_path, session_id="lane-2", cwd=tmp_path
    )
    assert outcome == "asked"
    (question,) = read_open_questions(tmp_path)
    _assert_fits(question)


def test_friction_lane_of_30_rows_fits(tmp_path: Path) -> None:
    from fno.agents.friction_lane import reconcile_friction

    rows = [
        (
            SimpleNamespace(
                name="CONTENDED",
                verdict="contended",
                row_id=f"row-{i}",
                basis="two live panes",
            ),
            SimpleNamespace(node=f"x-{i:06x}" if i % 2 else None),
        )
        for i in range(1, 31)
    ]
    outcome, _qid = reconcile_friction(
        rows, root=tmp_path, session_id="lane-3", cwd=tmp_path
    )
    assert outcome == "asked"
    (question,) = read_open_questions(tmp_path)
    _assert_fits(question)


# --- the king-wake ceiling markers -------------------------------------------


def test_king_wake_ceiling_markers_fit(tmp_path: Path) -> None:
    from fno.pr_watch import _king_wake as kw

    target = kw.CrownTarget(
        holder="king-1",
        scope="x-a792",
        root=tmp_path,
        manifest=tmp_path / "kings" / "x-a792.md",
    )
    qid = kw._ask_wake_ceiling(target, count=999, ceiling=10)
    (question,) = read_open_questions(tmp_path)
    assert question.id == qid
    assert question.node == "x-a792"
    _assert_fits(question)

    qid2 = kw._ask_respawn_ceiling(target, count=3, ceiling=3)
    questions = read_open_questions(tmp_path)
    assert {q.id for q in questions} == {qid, qid2}
    for q in questions:
        if q.id == qid2:
            assert q.node == "x-a792"
            _assert_fits(q)
