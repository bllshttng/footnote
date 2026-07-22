"""Integration tests for `fno plan brief` CLI command.

Uses CliRunner against the real typer app. Exercises exit codes and
output shape without requiring a running subprocess.

Covers:
- AC2-HP: exit 0, markdown output with required sections
- AC2-ERR: unknown task-id exits 2, stderr lists valid task-ids
- AC2-UI: --format json exits 0, output matches fixed schema
- AC2-EDGE: fail-open when no tagged entries
- AC2-FR: malformed Execution Strategy YAML exits 3
- Plan file not found exits 1
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()

FIXTURES = Path(__file__).parent.parent / "fixtures" / "plans"
SAMPLE_PLAN = FIXTURES / "lean_brief_sample.md"

MALFORMED_PLAN_CONTENT = """\
---
claims: ab-test-malformed
created: 2026-05-18
status: ready
type: think
feature: malformed plan
---

# Malformed Plan

## Overview

This plan has malformed Execution Strategy YAML.

## Acceptance Criteria

**AC2-HP:** Something.

## Locked Decisions (DO NOT revisit)

1. **Some decision.** *Why:* testing. *How to apply:* test.

## Failure Modes

**Boundaries**
- The system must handle this test case

## Execution Strategy

```yaml
tasks:
  - id: [unclosed list
    title: broken yaml here
```
"""

NO_EXEC_STRATEGY_PLAN_CONTENT = """\
---
claims: ab-test-noexec
created: 2026-05-18
status: ready
type: think
feature: no exec strategy
---

# No Exec Strategy Plan

## Overview

A plan without an Execution Strategy section.

## Failure Modes

**Boundaries**
- Something
"""


@pytest.fixture
def malformed_plan(tmp_path: Path) -> Path:
    p = tmp_path / "malformed.md"
    p.write_text(MALFORMED_PLAN_CONTENT, encoding="utf-8")
    return p


@pytest.fixture
def no_exec_plan(tmp_path: Path) -> Path:
    p = tmp_path / "no_exec.md"
    p.write_text(NO_EXEC_STRATEGY_PLAN_CONTENT, encoding="utf-8")
    return p


class TestBriefHappyPath:
    def test_ac2_hp_exit_0(self) -> None:
        """AC2-HP: valid task-id exits 0."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1"])
        assert result.exit_code == 0, f"stdout: {result.stdout}\n{result.exception}"

    def test_ac2_hp_markdown_has_project_context(self) -> None:
        """AC2-HP: output contains first overview paragraph."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1"])
        assert result.exit_code == 0
        assert "first paragraph of the overview" in result.stdout

    def test_ac2_hp_markdown_has_task_spec(self) -> None:
        """AC2-HP: output contains task title and surface files."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1"])
        assert result.exit_code == 0
        assert "CLI entry point" in result.stdout
        assert "brief.py" in result.stdout

    def test_ac2_hp_markdown_has_verify_command(self) -> None:
        """AC2-HP: output contains verify command."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1"])
        assert result.exit_code == 0
        assert "uv run pytest" in result.stdout

    def test_ac2_hp_markdown_has_acceptance_criteria(self) -> None:
        """AC2-HP: output contains at least one acceptance criterion."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1"])
        assert result.exit_code == 0
        assert "AC2-HP" in result.stdout or "AC2" in result.stdout

    def test_default_format_is_markdown(self) -> None:
        """Default --format is markdown (plain text, not JSON)."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1"])
        assert result.exit_code == 0
        # If markdown, not a JSON object at root
        assert not result.stdout.strip().startswith("{")


class TestBriefErrors:
    def test_ac2_err_unknown_task_exits_2(self) -> None:
        """AC2-ERR: unknown task-id exits 2."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "9.9"])
        assert result.exit_code == 2, f"stdout: {result.stdout}\nstderr: {result.output}"

    def test_ac2_err_stderr_lists_valid_task_ids(self) -> None:
        """AC2-ERR: stderr should mention valid task-ids."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "9.9"])
        # CliRunner merges stdout+stderr into output by default
        assert "1.1" in result.output or "2.1" in result.output, (
            f"Valid task-ids not in output: {result.output!r}"
        )

    def test_plan_not_found_exits_1(self) -> None:
        """Plan file not found exits 1."""
        result = runner.invoke(app, ["plan", "brief", "/nonexistent/path/plan.md", "--task", "1.1"])
        assert result.exit_code == 1, f"stdout: {result.stdout}\n{result.exception}"

    def test_ac2_fr_malformed_exec_strategy_exits_3(self, malformed_plan: Path) -> None:
        """AC2-FR: malformed Execution Strategy YAML exits 3."""
        result = runner.invoke(app, ["plan", "brief", str(malformed_plan), "--task", "1.1"])
        assert result.exit_code == 3, (
            f"Expected exit code 3, got {result.exit_code}. "
            f"output: {result.output}\n"
            f"exception: {result.exception}"
        )

    def test_missing_exec_strategy_section_exits_2(self, no_exec_plan: Path) -> None:
        """Plan missing Execution Strategy section exits 2."""
        result = runner.invoke(app, ["plan", "brief", str(no_exec_plan), "--task", "1.1"])
        assert result.exit_code == 2, f"output: {result.output}"


class TestBriefJsonFormat:
    def test_ac2_ui_json_exit_0(self) -> None:
        """AC2-UI: --format json exits 0."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1", "--format", "json"])
        assert result.exit_code == 0, f"stdout: {result.stdout}\n{result.exception}"

    def test_ac2_ui_json_parseable(self) -> None:
        """AC2-UI: output is valid JSON."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1", "--format", "json"])
        assert result.exit_code == 0
        data = json.loads(result.stdout)
        assert isinstance(data, dict)

    def test_ac2_ui_json_has_required_keys(self) -> None:
        """AC2-UI: JSON output has the fixed top-level schema."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1", "--format", "json"])
        data = json.loads(result.stdout)
        required = {
            "project_context", "task_spec", "acceptance_criteria",
            "locked_decisions", "failure_modes", "files", "patterns", "verify_command",
        }
        assert required <= set(data.keys()), (
            f"Missing keys: {required - set(data.keys())}"
        )

    def test_ac2_ui_json_task_spec_fields(self) -> None:
        """AC2-UI: task_spec sub-object has all required fields."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1", "--format", "json"])
        data = json.loads(result.stdout)
        ts = data["task_spec"]
        for field in ("id", "title", "surface", "verify", "acceptance", "notes"):
            assert field in ts, f"task_spec missing field: {field}"

    def test_ac2_ui_json_ac_entry_fields(self) -> None:
        """AC2-UI: acceptance_criteria entries have fixed fields."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1", "--format", "json"])
        data = json.loads(result.stdout)
        if data["acceptance_criteria"]:
            ac = data["acceptance_criteria"][0]
            for field in ("ac_type", "code", "text", "tags"):
                assert field in ac

    def test_ac2_ui_json_locked_decision_fields(self) -> None:
        """AC2-UI: locked_decisions entries have fixed fields."""
        result = runner.invoke(app, [
            "plan", "brief", str(SAMPLE_PLAN), "--task", "2.1",
            "--format", "json", "--include-locked-decisions", "all"
        ])
        data = json.loads(result.stdout)
        if data["locked_decisions"]:
            ld = data["locked_decisions"][0]
            for field in ("number", "title", "rationale", "application", "tags"):
                assert field in ld

    def test_ac2_ui_json_failure_mode_fields(self) -> None:
        """AC2-UI: failure_modes entries have fixed fields."""
        result = runner.invoke(app, [
            "plan", "brief", str(SAMPLE_PLAN), "--task", "2.1",
            "--format", "json", "--include-failure-modes", "all"
        ])
        data = json.loads(result.stdout)
        if data["failure_modes"]:
            fm = data["failure_modes"][0]
            for field in ("category", "bullet", "tags"):
                assert field in fm

    def test_ac2_ui_json_pattern_fields(self) -> None:
        """AC2-UI: patterns entries have fixed fields."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1", "--format", "json"])
        data = json.loads(result.stdout)
        if data["patterns"]:
            p = data["patterns"][0]
            for field in ("pattern", "source", "why"):
                assert field in p

    def test_ac2_ui_json_file_fields(self) -> None:
        """AC2-UI: files entries have fixed fields."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "2.1", "--format", "json"])
        data = json.loads(result.stdout)
        if data["files"]:
            f = data["files"][0]
            for field in ("path", "action", "notes"):
                assert field in f


class TestBriefEdgeCases:
    def test_ac2_edge_fail_open_no_tagged_entries(self) -> None:
        """AC2-EDGE: with --include-locked-decisions relevant and no tagged entries,
        all locked decisions are included (fail-open)."""
        result = runner.invoke(app, [
            "plan", "brief", str(SAMPLE_PLAN), "--task", "2.1",
            "--format", "json", "--include-locked-decisions", "relevant"
        ])
        assert result.exit_code == 0
        data = json.loads(result.stdout)
        # Sample plan has 2 locked decisions, all untagged
        # fail-open means all 2 should be included
        assert len(data["locked_decisions"]) == 2, (
            f"Expected 2 locked decisions (fail-open), got {len(data['locked_decisions'])}"
        )

    def test_include_failure_modes_none(self) -> None:
        """--include-failure-modes none produces empty failure_modes list."""
        result = runner.invoke(app, [
            "plan", "brief", str(SAMPLE_PLAN), "--task", "2.1",
            "--format", "json", "--include-failure-modes", "none"
        ])
        assert result.exit_code == 0
        data = json.loads(result.stdout)
        assert data["failure_modes"] == []

    def test_include_locked_decisions_none(self) -> None:
        """--include-locked-decisions none produces empty locked_decisions list."""
        result = runner.invoke(app, [
            "plan", "brief", str(SAMPLE_PLAN), "--task", "2.1",
            "--format", "json", "--include-locked-decisions", "none"
        ])
        assert result.exit_code == 0
        data = json.loads(result.stdout)
        assert data["locked_decisions"] == []

    def test_task_1_1_also_works(self) -> None:
        """Brief works for task 1.1 (not just 2.1)."""
        result = runner.invoke(app, ["plan", "brief", str(SAMPLE_PLAN), "--task", "1.1"])
        assert result.exit_code == 0
        assert "Foundation module" in result.stdout
