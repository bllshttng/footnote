"""Validator emits a WARN line when a plan doc has no `project:` field.

The validator stays warn-only on first ship; tighten to error in a follow-on.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
VALIDATOR = REPO_ROOT / "skills" / "blueprint" / "scripts" / "validate-plan.sh"


_INDEX_TEMPLATE = (
    "---\n"
    "plan: foo\n"
    "scope: feature\n"
    "{project_line}"
    "execution_mode: sequential\n"
    "---\n"
    "# Title\n\n"
    "## Critical Path Trace\n\n"
    "```yaml\nscope: feature\n```\n\n"
    "## Scope Classification\n\n"
    "```yaml\nscope: feature\n```\n\n"
    "## Phase Dependencies\n\n"
    "## Tasks\n## Acceptance Criteria\n## Steps\n## Files\n"
)


def _plan_minus_project(tmp_path: Path) -> Path:
    plan = tmp_path / "plan-no-project.md"
    plan.write_text(_INDEX_TEMPLATE.format(project_line=""))
    return plan


def _plan_with_project(tmp_path: Path) -> Path:
    plan = tmp_path / "plan-with-project.md"
    plan.write_text(
        _INDEX_TEMPLATE.format(project_line="project: example-pipeline\n")
    )
    return plan


def test_validate_plan_warns_on_missing_project_field(tmp_path):
    plan = _plan_minus_project(tmp_path)
    result = subprocess.run(
        ["bash", str(VALIDATOR), str(plan)],
        capture_output=True, text=True, cwd=str(REPO_ROOT),
    )
    assert "WARN" in result.stdout
    assert "missing 'project:' field" in result.stdout
    assert result.returncode == 0


def test_validate_plan_ok_on_present_project_field(tmp_path):
    plan = _plan_with_project(tmp_path)
    result = subprocess.run(
        ["bash", str(VALIDATOR), str(plan)],
        capture_output=True, text=True, cwd=str(REPO_ROOT),
    )
    assert "has 'project:' field" in result.stdout
