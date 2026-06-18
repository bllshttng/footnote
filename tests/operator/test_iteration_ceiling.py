"""
Tests for the full-stage-loop iteration ceiling in orchestrator.py.

The config.executors.impeccable.max_iterations_per_task (default 8) must apply
to the ENTIRE /impeccable stage loop, not per-stage. A single budget is shared
across all stage invocations (craft, critique, polish, harden, audit, ...).

AC1-HP: 3 stages + 1 extra critique = 4 iterations_used, well below 8.
AC2-HP: iteration 8 reached with score 30/40 -> DONE_WITH_CONCERNS (in band, not FAILED).
AC3-ERR: attempt to run 9 iterations -> budget fires at 8, iter 9 never starts.
AC4-EDGE: per-stage ceilings are NOT introduced; total budget is shared across stages.
"""
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).parent.parent.parent
sys.path.insert(0, str(REPO_ROOT / "skills" / "do"))

from orchestrator import (  # noqa: E402
    ImpeccableStageLoop,
    ImpeccableVerdict,
    IMPECCABLE_DEFAULT_MAX_ITERATIONS,
    IMPECCABLE_DEFAULT_CRITIQUE_TARGET,
    IMPECCABLE_DEFAULT_CRITIQUE_FLOOR,
)


# ---------------------------------------------------------------------------
# AC1-HP: Total iterations counted across stages
# ---------------------------------------------------------------------------

def test_ac1_hp_total_iterations_counted_across_stages():
    """AC1-HP: craft + critique(x2) + harden = 4 iterations_used total."""
    loop = ImpeccableStageLoop(max_iterations=8)

    loop.record_stage("craft")
    loop.record_stage("critique")
    loop.record_stage("critique")  # second critique pass
    loop.record_stage("harden")

    assert loop.iterations_used == 4, (
        f"Expected 4 total iterations across stages, got {loop.iterations_used}"
    )
    assert not loop.ceiling_reached, "4 iterations should not trip the ceiling at 8"


# ---------------------------------------------------------------------------
# AC1-HP variant: stages each count as one iteration
# ---------------------------------------------------------------------------

def test_each_stage_invocation_counts_as_one_iteration():
    """Every stage invocation increments the shared counter by 1."""
    loop = ImpeccableStageLoop(max_iterations=8)
    stages = ["craft", "critique", "harden"]
    for stage in stages:
        loop.record_stage(stage)
    assert loop.iterations_used == 3


# ---------------------------------------------------------------------------
# AC2-HP: Ceiling trips -> two-tier verdict, not hard FAILED
# ---------------------------------------------------------------------------

def test_ac2_hp_ceiling_at_8_with_score_30_yields_done_with_concerns():
    """AC2-HP: score 30/40 at iteration 8 ceiling -> DONE_WITH_CONCERNS (in band)."""
    loop = ImpeccableStageLoop(
        max_iterations=8,
        critique_target=IMPECCABLE_DEFAULT_CRITIQUE_TARGET,   # 35
        critique_floor=IMPECCABLE_DEFAULT_CRITIQUE_FLOOR,     # 25
    )
    for _ in range(8):
        loop.record_stage("critique")

    assert loop.ceiling_reached

    verdict = loop.compute_verdict(final_score=30)
    assert verdict == ImpeccableVerdict.DONE_WITH_CONCERNS, (
        f"Score 30 (in band 25-35) at ceiling should yield DONE_WITH_CONCERNS, got {verdict}"
    )


def test_ceiling_at_8_with_score_35_yields_success():
    """Score >= 35 at ceiling -> SUCCESS (meets target)."""
    loop = ImpeccableStageLoop(max_iterations=8)
    for _ in range(8):
        loop.record_stage("critique")

    verdict = loop.compute_verdict(final_score=35)
    assert verdict == ImpeccableVerdict.SUCCESS


def test_ceiling_at_8_with_score_24_yields_failed():
    """Score < floor (25) at ceiling -> FAILED."""
    loop = ImpeccableStageLoop(max_iterations=8)
    for _ in range(8):
        loop.record_stage("critique")

    verdict = loop.compute_verdict(final_score=24)
    assert verdict == ImpeccableVerdict.FAILED


# ---------------------------------------------------------------------------
# AC3-ERR: Budget fires at exactly N, iteration N+1 never starts
# ---------------------------------------------------------------------------

def test_ac3_err_ceiling_fires_at_max_iterations():
    """AC3-ERR: ceiling fires at exactly max_iterations; can_dispatch returns False after."""
    loop = ImpeccableStageLoop(max_iterations=5)

    for i in range(5):
        assert loop.can_dispatch(), f"Should be able to dispatch on iteration {i+1}"
        loop.record_stage("critique")

    # After 5 iterations, ceiling is reached
    assert loop.ceiling_reached
    assert not loop.can_dispatch(), "Iteration 6 must be blocked by ceiling"


def test_iteration_budget_not_per_stage():
    """Budget is shared: 8 total, not 8 per stage type."""
    loop = ImpeccableStageLoop(max_iterations=8)

    # Alternate craft and critique - if budget were per-stage, each could run 8 times.
    # With shared budget, total is capped at 8.
    stages_run = 0
    for i in range(10):
        if not loop.can_dispatch():
            break
        stage = "craft" if i % 2 == 0 else "critique"
        loop.record_stage(stage)
        stages_run += 1

    assert stages_run == 8, (
        f"Shared budget should cap total at 8 regardless of stage type, got {stages_run}"
    )
    assert loop.ceiling_reached


# ---------------------------------------------------------------------------
# AC4-EDGE: Default values match documented contract
# ---------------------------------------------------------------------------

def test_ac4_edge_default_max_iterations_is_8():
    """Default max_iterations_per_task must be 8 per documented contract."""
    assert IMPECCABLE_DEFAULT_MAX_ITERATIONS == 8


def test_ac4_edge_default_critique_target_is_35():
    """Default critique_target must be 35 per documented contract."""
    assert IMPECCABLE_DEFAULT_CRITIQUE_TARGET == 35


def test_ac4_edge_default_critique_floor_is_25():
    """Default critique_floor must be 25 per documented contract."""
    assert IMPECCABLE_DEFAULT_CRITIQUE_FLOOR == 25


# ---------------------------------------------------------------------------
# Documentation check: executor-resolution.md records single-budget contract
# ---------------------------------------------------------------------------

def test_executor_resolution_doc_mentions_single_budget_contract():
    """executor-resolution.md must document the single-budget contract for impeccable."""
    doc = (REPO_ROOT / "skills" / "do" / "references" / "executor-resolution.md").read_text()
    assert "single" in doc.lower() and "budget" in doc.lower(), (
        "executor-resolution.md must document the single-budget contract"
    )
    assert "iterations_used" in doc or "iterations used" in doc.lower(), (
        "executor-resolution.md must mention the iterations_used counter"
    )


def test_executor_resolution_doc_mentions_two_tier_verdict_at_ceiling():
    """executor-resolution.md must state two-tier verdict is the exit when ceiling trips."""
    doc = (REPO_ROOT / "skills" / "do" / "references" / "executor-resolution.md").read_text()
    assert "two-tier" in doc.lower() or "done_with_concerns" in doc.lower(), (
        "executor-resolution.md must document two-tier verdict as canonical ceiling exit"
    )
    assert "failed" not in doc.lower().split("not")[0].split("never")[0] or "done_with_concerns" in doc.lower(), (
        "executor-resolution.md must clarify DONE_WITH_CONCERNS, not hard FAILED, at ceiling"
    )
