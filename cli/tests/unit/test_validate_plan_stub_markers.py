"""Validator refuses to pass an unfilled decompose-child scaffold (x-edf7 US1/US4).

A `blueprint decompose` child is born `status: stub` with placeholder markers +
an empty-why sentinel; linking its plan_path flips it `ready` and dispatchers
launch against it. The validator is the gate: it must reject any plan still
carrying a stub marker (naming it) and require a non-empty `## Why (from epic)`
on group-child plans. Generating the scaffold from the real writer keeps the
test and cli/src/fno/graph/_decompose.py from drifting.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

from fno.graph._decompose import scaffold_separate_plan, validate_groups

REPO_ROOT = Path(__file__).resolve().parents[3]
VALIDATOR = REPO_ROOT / "skills" / "blueprint" / "scripts" / "validate-plan.sh"


def _run(plan: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(VALIDATOR), str(plan)],
        capture_output=True, text=True, cwd=str(REPO_ROOT),
    )


def _grp():
    return validate_groups([{"slug": "1", "title": "Group 1", "waves": "1-2"}], None)[0]


_FILLED = """---
title: Group 1
status: ready
kind: quick-plan
parent_epic: ab-epic0001
source_doc: big.md
---

# Group 1

## Why (from epic)

Gate launch on a real plan so dispatchers stop stampeding stub scaffolds.

## Context

Real context here.

## Changes

Add the guard in foo.py.

## Files to Modify

- foo.py

## Verification

pytest cli/tests
"""


def test_validator_rejects_unfilled_scaffold(tmp_path):
    plan = tmp_path / "big.group-1.md"
    plan.write_text(scaffold_separate_plan(_grp(), "ab-epic0001", "big.md", why_digest=""))
    result = _run(plan)
    assert result.returncode == 1, result.stdout
    assert "unfilled stub marker" in result.stdout
    assert "Seeded from epic waves" in result.stdout  # marker is named


def test_validator_flags_empty_why_on_group_child(tmp_path):
    # A group child (parent_epic set) whose ## Why body is empty is rejected even
    # if the other markers were cleared.
    plan = tmp_path / "big.group-1.md"
    plan.write_text(
        "---\ntitle: G\nparent_epic: ab-epic0001\n---\n\n"
        "# G\n\n## Why (from epic)\n\n## Changes\n\nreal\n"
    )
    result = _run(plan)
    assert result.returncode == 1, result.stdout
    assert "empty '## Why (from epic)'" in result.stdout


def test_validator_passes_filled_child(tmp_path):
    plan = tmp_path / "big.group-1.md"
    plan.write_text(_FILLED)
    result = _run(plan)
    assert result.returncode == 0, result.stdout
    assert "no unfilled stub markers" in result.stdout
    assert "## Why (from epic) is non-empty" in result.stdout


def test_validator_ignores_why_on_non_group_plan(tmp_path):
    # A normal quick-plan (no parent_epic) is not forced to grow a Why section.
    plan = tmp_path / "normal.md"
    plan.write_text(
        "---\ntitle: X\nproject: fno\n---\n\n# X\n\n"
        "### Task 1.1: do it\n\n**Files**: foo.py\n**Verify**: pytest\n"
    )
    result = _run(plan)
    assert result.returncode == 0, result.stdout
