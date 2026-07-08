#!/usr/bin/env python3
"""Tests for impeccable_stages validator in validate-plan.sh (Phase 02.2).

Acceptance criteria:
  AC1-HP:  impeccable_stages: [craft, critique, harden] -> passes silently
  AC2-HP:  impeccable_stages: [delight, layout] (pin-only) -> passes
  AC3-ERR: impeccable_stages: [craft, foo, harden] -> exit 1, names 'foo'
  AC4-EDGE: impeccable_stages: [] -> exit 1, "empty list"

Run: python3 -m pytest tests/spec/test_impeccable_stages_validator.py -v
"""
import subprocess
import textwrap
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
VALIDATOR = REPO_ROOT / "skills" / "spec" / "scripts" / "validate-plan.sh"


def _make_plan(plan_dir: Path, stage_lines: str = "") -> None:
    """Write a minimal valid plan folder for the validator."""
    (plan_dir / "00-INDEX.md").write_text(
        textwrap.dedent("""\
        ---
        title: Test Plan
        scope: feature
        project: test
        execution_mode: sequential
        waves:
          - wave: 1
            mode: sequential
            tasks: [1.1]
        ---

        # Test Plan

        ## Critical Path Trace

        User opens page -> Task 1.1 shows result

        ## Scope Classification

        ```yaml
        scope: feature
        ```
        """)
    )
    # Write a phase file that includes the impeccable_stages entry under test
    phase_content = textwrap.dedent(f"""\
    ---
    phase: 1
    title: Implementation
    ---

    # Phase 1

    ### Task 1.1: Build Component

    **Files:** `src/components/Foo.tsx`

    Steps:
    1. Implement component

    Acceptance Criteria:
    - AC1-HP: Given..., when..., then...

    {stage_lines}
    """)
    (plan_dir / "01-implementation.md").write_text(phase_content)


def _run_validator(plan_dir: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(VALIDATOR), str(plan_dir)],
        capture_output=True,
        text=True,
    )


def test_ac1_hp_valid_standard_stages():
    """AC1-HP: [craft, critique, harden] -> validator passes."""
    with tempfile.TemporaryDirectory() as tmp:
        plan_dir = Path(tmp) / "my-plan"
        plan_dir.mkdir()
        _make_plan(plan_dir, "impeccable_stages: [craft, critique, harden]")

        result = _run_validator(plan_dir)

        assert result.returncode == 0, (
            f"Expected exit 0 for valid stages.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        # Section header should appear
        assert "impeccable_stages" in result.stdout


def test_ac2_hp_pin_only_stages_pass():
    """AC2-HP: [delight, layout] (pin-only treatments) -> passes."""
    with tempfile.TemporaryDirectory() as tmp:
        plan_dir = Path(tmp) / "my-plan"
        plan_dir.mkdir()
        _make_plan(plan_dir, "impeccable_stages: [delight, layout]")

        result = _run_validator(plan_dir)

        assert result.returncode == 0, (
            f"Expected exit 0 for pin-only stages.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )


def test_ac3_err_unknown_stage_exits_1():
    """AC3-ERR: [craft, foo, harden] -> exit 1, message names 'foo'."""
    with tempfile.TemporaryDirectory() as tmp:
        plan_dir = Path(tmp) / "my-plan"
        plan_dir.mkdir()
        _make_plan(plan_dir, "impeccable_stages: [craft, foo, harden]")

        result = _run_validator(plan_dir)

        assert result.returncode == 1, (
            f"Expected exit 1 for unknown stage.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        combined = result.stdout + result.stderr
        assert "foo" in combined.lower(), f"Expected 'foo' in output. Got:\n{combined}"


def test_ac4_edge_empty_stages_exits_1():
    """AC4-EDGE: impeccable_stages: [] -> exit 1 with 'empty list' message."""
    with tempfile.TemporaryDirectory() as tmp:
        plan_dir = Path(tmp) / "my-plan"
        plan_dir.mkdir()
        _make_plan(plan_dir, "impeccable_stages: []")

        result = _run_validator(plan_dir)

        assert result.returncode == 1, (
            f"Expected exit 1 for empty stages list.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        combined = result.stdout + result.stderr
        assert "empty" in combined.lower(), f"Expected 'empty' in output. Got:\n{combined}"


def test_no_impeccable_stages_field_passes():
    """Tasks without impeccable_stages field pass silently (opt-in field)."""
    with tempfile.TemporaryDirectory() as tmp:
        plan_dir = Path(tmp) / "my-plan"
        plan_dir.mkdir()
        _make_plan(plan_dir, "")  # no impeccable_stages at all

        result = _run_validator(plan_dir)

        assert result.returncode == 0, (
            f"Expected exit 0 when impeccable_stages absent.\nstdout:\n{result.stdout}"
        )


def test_ac5_block_list_unknown_stage_exits_1():
    """AC5-ERR: block-list form with unknown stage -> exit 1, message names the unknown stage.

    NOTE: stage_lines is indented with 6 spaces so that after textwrap.dedent (which strips
    the 4-space common indent of the _make_plan template) the block-list items land with
    2-space indent under the key, producing valid YAML:
        impeccable_stages:
          - craft
          - foo
    """
    with tempfile.TemporaryDirectory() as tmp:
        plan_dir = Path(tmp) / "my-plan"
        plan_dir.mkdir()
        _make_plan(
            plan_dir,
            "impeccable_stages:\n      - craft\n      - foo",
        )

        result = _run_validator(plan_dir)

        assert result.returncode == 1, (
            f"Expected exit 1 for unknown stage in block-list form.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        combined = result.stdout + result.stderr
        import re as _re
        assert _re.search(r"unknown.*foo", combined, _re.IGNORECASE), (
            f"Expected 'unknown.*foo' pattern in output. Got:\n{combined}"
        )


def test_all_known_stages_pass():
    """All 18 known stages should pass validation."""
    all_stages = (
        "craft, critique, polish, harden, audit, layout, "
        "animate, bolder, colorize, delight, overdrive, quieter, typeset, "
        "distill, extract, adapt, shape, teach"
    )
    with tempfile.TemporaryDirectory() as tmp:
        plan_dir = Path(tmp) / "my-plan"
        plan_dir.mkdir()
        _make_plan(plan_dir, f"impeccable_stages: [{all_stages}]")

        result = _run_validator(plan_dir)

        assert result.returncode == 0, (
            f"Expected exit 0 for all known stages.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )


def test_inline_with_trailing_comment_passes():
    """Gemini fix (PR #217 round 1): trailing comment after the closing ]
    must not poison parsing. Without the fix, the sed pipeline's `tr -d '[]'`
    would leave the comment text in stages_raw and the comment's words would
    be tokenized as 'unknown stages'."""
    with tempfile.TemporaryDirectory() as tmp:
        plan_dir = Path(tmp) / "my-plan"
        plan_dir.mkdir()
        _make_plan(plan_dir, "impeccable_stages: [craft, harden]  # default + close")

        result = _run_validator(plan_dir)

        assert result.returncode == 0, (
            f"Expected exit 0 for inline list with trailing comment.\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        # Sanity: validator should NOT misread the comment words as stage entries
        combined = result.stdout + result.stderr
        assert "unknown" not in combined.lower() or "default" not in combined.lower(), (
            f"Validator misread trailing comment as stages.\n{combined}"
        )


def test_inline_with_trailing_comment_still_catches_unknown():
    """Trailing comment must not LET unknown stages slip through either."""
    with tempfile.TemporaryDirectory() as tmp:
        plan_dir = Path(tmp) / "my-plan"
        plan_dir.mkdir()
        _make_plan(plan_dir, "impeccable_stages: [craft, foo]  # has a typo")

        result = _run_validator(plan_dir)

        assert result.returncode == 1, (
            f"Expected exit 1 - unknown 'foo' in inline list with trailing comment.\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        combined = result.stdout + result.stderr
        import re as _re
        assert _re.search(r"unknown.*foo", combined, _re.IGNORECASE), (
            f"Expected 'unknown.*foo' in output. Got:\n{combined}"
        )
