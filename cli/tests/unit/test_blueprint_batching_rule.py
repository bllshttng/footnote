"""x-c1c7: batch blueprints, up to three per session, one plan per shape.

Two checks, because they answer different questions.

Static, blocking: the king skill's dispatch step states the number (a rule
without a number is advice nobody applies), and the undispatched board queue
carries the batching note at the exact moment a king picks what to dispatch.
Both assert a positive marker, never an absence.

Behavioral, advisory: `evals/bank/capability-blueprint-batching.yaml` hands a
fresh worker five nodes, two sharing a shape, with no operator guidance. The
worker writes one line per intended blueprint dispatch to `dispatch-plan.md`.
This grader only runs when that artifact exists, and asserts exactly three
lines with the shape pair on one of them - a worker that emits five lines has
not received the rule.
"""
from __future__ import annotations

import re
from pathlib import Path

import pytest

from fno.king.board import BoardInputs, SourceRead, build_board

REPO_ROOT = Path(__file__).resolve().parents[3]
KING_SKILL = REPO_ROOT / "skills" / "king-for-a-day" / "SKILL.md"
# The eval worker runs with `--cwd <disposable-worktree>` and writes the
# artifact relative to that cwd, which IS repo root inside the graded
# worktree - never a fixed evals/runs/ path (grading.py's _grade_exit also
# runs with cwd=workdir, so this resolves to the same worktree at grade time).
DISPATCH_PLAN = REPO_ROOT / "dispatch-plan.md"


def _section_3d(text: str) -> str:
    match = re.search(
        r"\*\*3d\..*?(?=\n### 4\. Kick off)", text, re.S
    )
    assert match, "3d batching block not found in king-for-a-day/SKILL.md"
    return match.group(0)


def test_3d_states_the_number_three():
    section = _section_3d(KING_SKILL.read_text())
    assert re.search(r"\bthree\b|\b3\b", section, re.I), (
        "3d block names no count; a rule without a number is advice nobody applies"
    )


def test_3d_names_the_supersede_verb():
    section = _section_3d(KING_SKILL.read_text())
    assert "fno backlog supersede" in section


def _ok(payload):
    return SourceRead(payload=payload)


def _node(node_id, priority="p0", plan_path="/plans/p.md"):
    return {"id": node_id, "priority": priority, "plan_path": plan_path}


def test_undispatched_queue_carries_a_nonempty_note():
    inputs = BoardInputs(
        ready=_ok([_node("x-1234")]),
        claims=_ok([]),
        claimed_nodes=_ok([]),
        holder_activity={},
        prs=_ok([]),
        questions=_ok([]),
        needs=_ok([]),
    )
    board = build_board(inputs)
    queue = next(q for q in board["queues"] if q["name"] == "undispatched")
    assert queue["note"], "undispatched queue has no batching note"
    assert "3" in queue["note"] or "three" in queue["note"].lower()


@pytest.mark.skipif(not DISPATCH_PLAN.exists(), reason="behavioral artifact not present; run the eval first")
def test_behavioral_dispatch_plan_batches_the_shape_pair():
    lines = [ln for ln in DISPATCH_PLAN.read_text().splitlines() if ln.strip()]
    assert len(lines) == 3, (
        f"expected 3 dispatch lines (batched), got {len(lines)}: a worker that "
        "emits five lines has not received the rule"
    )
    paired = [ln for ln in lines if "x-76d1" in ln and "x-97eb" in ln]
    assert paired, "the x-76d1/x-97eb shape pair must share one dispatch line"
