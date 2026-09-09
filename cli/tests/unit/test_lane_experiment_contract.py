"""The x-fd52 paired-task experiment contract: AC4-HP, AC4-EDGE.

Ties the wave-4 bank fixtures (capability-lane-*.yaml) to the wave-1/2
lane machinery: the same fixtures, repeats, and stopping rule must permit a
reproducible comparison (AC4-HP), and account/budget/identity refusals must
say so explicitly rather than fabricate quality evidence (AC4-EDGE). See
docs/architecture/lane-qualification.md for the operator-facing recipe.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.evals.bank import BankError, LaneCoordinate, LaneError, discover_bank, resolve_lane
from fno.evals.report import CohortSpec, compare_cohorts
from fno.evals.runner import _lane_evidence

REPO_ROOT = Path(__file__).resolve().parents[3]
BANK_DIR = REPO_ROOT / "evals" / "bank"
LANE_TASK_IDS = (
    "capability-lane-blueprint",
    "capability-lane-implementation",
    "capability-lane-review",
)


def _require_seed_bank() -> dict:
    if not BANK_DIR.is_dir():
        pytest.skip("seed bank not present in this checkout")
    return {t.id: t for t in discover_bank(BANK_DIR)}


# --------------------------------------------------------------------------- #
# the three paired tasks are real, mechanical, capability-tier fixtures
# --------------------------------------------------------------------------- #

def test_paired_tasks_exist_capability_tier_with_real_grades() -> None:
    by_id = _require_seed_bank()
    for task_id in LANE_TASK_IDS:
        task = by_id.get(task_id)
        assert task is not None, f"{task_id} missing from evals/bank/"
        assert task.tier == "capability"
        assert task.prompt, f"{task_id} must be prompt-bearing (not grade-only)"
        assert len(task.grade) >= 2, f"{task_id} grade must be more than a single check"
        assert "lane-qualification" in task.tags


def test_bank_task_load_is_never_a_bare_ok_grade() -> None:
    """A gradeless or all-trivial fixture would be exactly the synthetic
    quality-evidence failure this bank exists to prevent (docs/evals.md)."""
    by_id = _require_seed_bank()
    for task_id in LANE_TASK_IDS:
        for check in by_id[task_id].grade:
            if check.kind == "exit":
                assert (check.command or "").strip() not in ("true", ":", ""), (
                    f"{task_id}: a trivial exit-only check is a decorative grade"
                )


# --------------------------------------------------------------------------- #
# AC4-HP: the same fixture, repeats and stopping rule permit a reproducible
# comparison across configuration fingerprints
# --------------------------------------------------------------------------- #

def _lane_row(cohort: str, lane: str, passed: bool, rev: str = "fixture-rev-1") -> dict:
    return {"task_id": "capability-lane-implementation", "tier": "capability",
            "pass": passed, "experiment_id": cohort, "requested_lane": lane,
            "lane_status": "ok", "bank_rev": rev, "duration_s": 1.0}


def test_same_fixture_and_repeats_yield_a_reproducible_cohort_comparison() -> None:
    rows = [_lane_row("baseline", "claude-sonnet", True) for _ in range(5)]
    rows += [_lane_row("candidate", "astra-high", True) for _ in range(5)]
    cohorts = [
        CohortSpec("baseline", repeats=5, fixture_rev="fixture-rev-1"),
        CohortSpec("candidate", repeats=5, fixture_rev="fixture-rev-1"),
    ]
    out = compare_cohorts(rows, cohorts, promotion_criteria={
        "baseline": "baseline", "candidate": "candidate", "min_pass_at_1": 1.0,
    })
    b, c = out["cohorts"]["baseline"], out["cohorts"]["candidate"]
    assert b["sample_count"] == c["sample_count"] == 5
    assert b["fixture_revisions"] == c["fixture_revisions"] == ["fixture-rev-1"]
    assert not b["fixture_rev_mismatch"] and not c["fixture_rev_mismatch"]
    assert out["promotion"]["recommended"] is True


# --------------------------------------------------------------------------- #
# AC4-EDGE: identity mismatch, unavailable model/profile, malformed input,
# and a budget/access-bound trial all say so explicitly - never a promoted
# or graded claim built on unrun/misattributed work.
# --------------------------------------------------------------------------- #

def test_identity_mismatch_is_substituted_not_folded_into_the_requested_lane() -> None:
    lane = LaneCoordinate(name="astra-high", harness="codex", model="gpt-6-astra",
                          effort="high", route="", account="")
    observed = {"harness": "claude", "model": "claude-sonnet-5", "effort": "medium"}
    evidence = _lane_evidence(lane, observed)
    assert evidence["substituted"] is True
    assert evidence["lane_status"] == "substituted"


def test_unavailable_model_profile_refuses_by_name() -> None:
    with pytest.raises(LaneError, match="unknown lane"):
        resolve_lane("no-such-lane-in-config")


def test_malformed_bank_task_refuses_loudly_naming_id_and_file(tmp_path: Path) -> None:
    bad = tmp_path / "broken.yaml"
    bad.write_text("id: broken\ntier: capability\ngrade: []\n", encoding="utf-8")
    with pytest.raises(BankError, match="broken"):
        discover_bank(tmp_path)


def test_budget_bound_trial_reports_unavailable_never_a_synthetic_pass() -> None:
    """No account/budget could exercise the candidate lane at all: zero rows.
    The comparison must say so, never silently score 100% of nothing."""
    rows = [_lane_row("baseline", "claude-sonnet", True) for _ in range(3)]
    out = compare_cohorts(
        rows, [CohortSpec("baseline", repeats=3), CohortSpec("candidate", repeats=3, budget_usd=0.0)],
        promotion_criteria={"baseline": "baseline", "candidate": "candidate"},
    )
    candidate = out["cohorts"]["candidate"]
    assert candidate["sample_count"] == 0
    assert candidate["pass_at_1"] is None  # never a fabricated rate
    assert out["promotion"]["recommended"] is False
    assert any("no scored samples" in r for r in out["promotion"]["reasons"])
