from __future__ import annotations

import json
from pathlib import Path

from typer.testing import CliRunner

from fno.plan._doc import load_plan
from fno.plan.cli import plan_app
from fno.plan.execution_validation import validate_execution


runner = CliRunner()


def _write_plan(tmp_path: Path, body: str, *, frontmatter: str = "") -> Path:
    plan = tmp_path / "plan.md"
    plan.write_text(
        "---\n"
        + (frontmatter or "status: ready\nkind: quick-plan\ncreated: 2026-07-25\n")
        + "---\n\n# Plan\n\n"
        + body,
        encoding="utf-8",
    )
    return plan


def _full_plan(strategy: str) -> str:
    return f"""## Overview

Executable plan.

## Execution Strategy

```yaml
{strategy}```
"""


VALID_STRATEGY = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    name: Build
    tasks: [\"1.1\"]
tasks:
  - id: \"1.1\"
    title: Build the validator
    surface: [cli/src/fno/plan/cli.py]
    verify: fno test cli/tests/unit/test_plan_execution_validation.py
    acceptance: [AC1-HP]
"""


def test_valid_strategy_has_no_representation_warnings(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, _full_plan(VALID_STRATEGY))

    result = validate_execution(load_plan(plan))

    assert result.violations == []


def test_quick_plan_uses_sections_not_task_headings(tmp_path: Path) -> None:
    plan = _write_plan(
        tmp_path,
        """## Context

Why this is needed.

## Changes

### 1. Change

Do the work.

## Files to Modify

| File | Action |
|---|---|
| `src/a.py` | modify |

## Verification

1. `fno test cli/tests/unit/test_a.py`
""",
    )

    assert validate_execution(load_plan(plan)).violations == []


def test_quick_plan_reports_missing_semantic_section(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, "## Context\n\nOnly context.\n")

    fields = {v.field for v in validate_execution(load_plan(plan)).violations}

    assert fields == {"Changes", "Files to Modify", "Verification"}


def test_placeholder_task_fields_fail_with_exact_paths(tmp_path: Path) -> None:
    strategy = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    tasks: [\"1.1\"]
tasks:
  - id: \"1.1\"
    title: Implement feature
    surface: []
    verify: \"# fill in verify command\"
    acceptance: []
"""
    plan = _write_plan(tmp_path, _full_plan(strategy))

    fields = {v.field for v in validate_execution(load_plan(plan)).violations}

    assert "tasks.1.1.surface" in fields
    assert "tasks.1.1.verify" in fields
    assert "tasks.1.1.acceptance" in fields


def test_duplicate_task_and_unknown_wave_reference_fail(tmp_path: Path) -> None:
    strategy = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    tasks: [\"1.1\", \"9.9\"]
tasks:
  - id: \"1.1\"
    title: First
    surface: [src/a.py]
    verify: fno test tests/test_a.py
    acceptance: [AC1]
  - id: \"1.1\"
    title: Duplicate
    surface: [src/b.py]
    verify: fno test tests/test_b.py
    acceptance: [AC2]
"""
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "duplicate task id" in messages
    assert "unknown task '9.9'" in messages


def test_task_collections_must_be_yaml_lists(tmp_path: Path) -> None:
    strategy = VALID_STRATEGY.replace(
        "surface: [cli/src/fno/plan/cli.py]",
        "surface: cli/src/fno/plan/cli.py",
    )
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "task '1.1' surface must be a list" in messages


def test_parallel_wave_rejects_shared_surface(tmp_path: Path) -> None:
    strategy = """execution_mode: parallel
waves:
  - wave: 1
    mode: parallel
    tasks: [\"1.1\", \"1.2\"]
tasks:
  - id: \"1.1\"
    title: First
    surface: [src/shared.py]
    verify: fno test tests/test_a.py
    acceptance: [AC1]
  - id: \"1.2\"
    title: Second
    surface: [src/shared.py]
    verify: fno test tests/test_b.py
    acceptance: [AC2]
"""
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "parallel tasks share surface 'src/shared.py'" in messages


def test_wave_dependency_cycle_fails(tmp_path: Path) -> None:
    strategy = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    depends_on: 2
    tasks: [\"1.1\"]
  - wave: 2
    mode: sequential
    depends_on: 1
    tasks: [\"2.1\"]
tasks:
  - id: \"1.1\"
    title: First
    surface: [src/a.py]
    verify: fno test tests/test_a.py
    acceptance: [AC1]
  - id: \"2.1\"
    title: Second
    surface: [src/b.py]
    verify: fno test tests/test_b.py
    acceptance: [AC2]
"""
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "dependency cycle" in messages


def test_execution_cli_text_and_json(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, _full_plan(VALID_STRATEGY))

    text_result = runner.invoke(plan_app, ["validate", str(plan), "--execution"])
    json_result = runner.invoke(plan_app, ["validate", str(plan), "--execution", "--json"])

    assert text_result.exit_code == 0
    assert text_result.stdout == f"valid execution: {plan}\n"
    payload = json.loads(json_result.stdout)
    assert payload == {
        "valid": True,
        "path": str(plan),
        "scope": "execution",
        "violations": [],
        "warnings": [],
    }


def test_execution_cli_fails_nonzero_with_field_report(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, "## Context\n\nOnly context.\n")

    result = runner.invoke(plan_app, ["validate", str(plan), "--execution"])

    assert result.exit_code == 1
    assert "invalid execution:" in result.output
    assert "Changes:" in result.output


def test_default_cli_remains_frontmatter_only(tmp_path: Path) -> None:
    plan = _write_plan(
        tmp_path,
        "## Context\n\nExecution-invalid but irrelevant to default validation.\n",
        frontmatter="node: x-test\nstatus: ready\ncreated: 2026-07-25\n",
    )

    result = runner.invoke(plan_app, ["validate", str(plan)])

    assert result.exit_code == 0
    assert result.stdout == f"valid: {plan}\n"
