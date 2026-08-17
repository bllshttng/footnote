"""Validator consolidation-block gate: negative and grandfather paths.

The happy path lives in the stub-marker and project-warning fixtures. These
tests pin the failure modes a silent gate would miss: a new plan with no
block, a legacy plan grandfathered to a warn, flow-style lists, comment
noise, and an outcome with no recorded decision.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
VALIDATOR = REPO_ROOT / "scripts" / "validate-plan.sh"


def _run(plan: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", str(VALIDATOR), str(plan)],
        capture_output=True,
        text=True,
        cwd=str(REPO_ROOT),
    )


def _plan(frontmatter: str) -> str:
    # The full-body shape the execution-contract leg accepts (mirrors the
    # stub-marker suite's _FILLED fixture), so these tests isolate the
    # consolidation gate rather than re-tripping the execution contract.
    return (
        f"---\n{frontmatter}---\n\n# T\n\n"
        "## Context\n\nReal context here.\n\n"
        "## Changes\n\nAdd the guard in foo.py.\n\n"
        "## Files to Modify\n\n- foo.py\n\n"
        "## Verification\n\npytest cli/tests\n"
    )


def test_new_plan_without_block_errors(tmp_path):
    plan = tmp_path / "new.md"
    plan.write_text(_plan("title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-18\n"))
    result = _run(plan)
    assert result.returncode == 1, result.stdout
    assert "no consolidation: block" in result.stdout
    # F2: the missing block must not abort the validator - later checks and
    # the summary still run.
    assert "Errors:" in result.stdout


def test_legacy_plan_without_block_warns_not_errors(tmp_path):
    plan = tmp_path / "old.md"
    plan.write_text(_plan("title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-15\n"))
    result = _run(plan)
    assert result.returncode == 0, result.stdout
    assert "backfill one before the next blueprint" in result.stdout


def test_flow_style_id_list_errors_with_fix(tmp_path):
    plan = tmp_path / "flow.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-18\n"
        "consolidation:\n"
        "  outcome: absorb\n"
        "  absorbed: [{id: x-ab12, reason: same lock}]\n"
    ))
    result = _run(plan)
    assert result.returncode == 1, result.stdout
    assert "block style" in result.stdout


def test_inline_comment_on_outcome_line_is_stripped(tmp_path):
    plan = tmp_path / "comment.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-18\n"
        "consolidation:\n"
        "  outcome: proceed_alone  # no siblings found\n"
        "  proceed_alone_against: []\n"
    ))
    result = _run(plan)
    assert result.returncode == 0, result.stdout
    assert "consolidation outcome is proceed_alone" in result.stdout


def test_absorb_with_empty_absorbed_list_errors(tmp_path):
    plan = tmp_path / "empty-absorb.md"
    plan.write_text(_plan(
        "title: T\nstatus: ready\nkind: quick-plan\ncreated: 2026-08-18\n"
        "consolidation:\n"
        "  outcome: absorb\n"
        "  absorbed: []\n"
    ))
    result = _run(plan)
    assert result.returncode == 1, result.stdout
    assert "absorbed: list is empty" in result.stdout


# -- the Pydantic shape authority (fno plan validate must agree with the gate) --


def test_pydantic_model_rejects_malformed_block():
    from pydantic import ValidationError

    from fno.plan.schema import PlanFrontmatter

    base = {"node": "x-1", "status": "ready", "created": "2026-08-18"}

    # Out-of-enum outcome fails the model, not just the bash gate.
    bad = dict(base, consolidation={"outcome": "maybe"})
    with pytest.raises(ValidationError):
        PlanFrontmatter.model_validate(bad)

    # An entry with an empty reason fails on both surfaces.
    bad = dict(base, consolidation={
        "outcome": "absorb",
        "absorbed": [{"id": "x-ab12", "reason": ""}],
    })
    with pytest.raises(ValidationError):
        PlanFrontmatter.model_validate(bad)

    # A well-formed block validates.
    good = dict(base, consolidation={
        "outcome": "absorb",
        "absorbed": [{"id": "x-ab12", "reason": "same lock"}],
        "reversal": "fno backlog unsupersede x-ab12",
    })
    assert PlanFrontmatter.model_validate(good).consolidation.outcome == "absorb"

    # Absorb recording no decision is rejected here too (the empty-block rule).
    bad = dict(base, consolidation={"outcome": "absorb", "absorbed": []})
    with pytest.raises(ValidationError):
        PlanFrontmatter.model_validate(bad)
