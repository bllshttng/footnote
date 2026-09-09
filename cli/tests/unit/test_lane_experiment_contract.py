"""The paired-task experiment contract: real fixtures, and lane refusals
that say so explicitly rather than fabricate quality evidence.

See docs/architecture/lane-qualification.md for the operator-facing recipe.
Cohort aggregation across paired runs is deferred to a follow-up.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.evals.bank import BankError, LaneError, discover_bank, resolve_lane
from fno.evals.runner import _lane_evidence
from fno.route_resolve import InventoryRow

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
# identity mismatch, unavailable lane, and malformed input all say so
# explicitly - never a graded claim built on unrun or misattributed work.
# --------------------------------------------------------------------------- #

def test_identity_mismatch_is_substituted_not_folded_into_the_requested_lane() -> None:
    lane = InventoryRow(name="astra-high", harness="codex", model="gpt-6-astra", effort="high")
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
