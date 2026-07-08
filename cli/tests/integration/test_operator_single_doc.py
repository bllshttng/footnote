"""Integration tests for orchestrator single-doc plan support.

Covers:
- AC4-EDGE (single-doc): single-doc plan with ## Execution Strategy YAML
  parses to the expected wave/task structure
- AC4-FR: missing/unreadable plan file -> structured failure with blocked_reason
- Malformed Execution Strategy YAML -> exit 3 with line/section info
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from textwrap import dedent

import pytest

# Add the orchestrator's parent to path so we can import it directly.
_ORCHESTRATOR_DIR = Path(__file__).resolve().parents[3] / "skills" / "do"
sys.path.insert(0, str(_ORCHESTRATOR_DIR))

# Add the fno package to path
_CLI_SRC = Path(__file__).resolve().parents[3] / "cli" / "src"
sys.path.insert(0, str(_CLI_SRC))

from orchestrator import (  # noqa: E402
    ExecutionStrategy,
    Wave,
    load_plan_strategy,
)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

MINIMAL_STRATEGY_YAML = dedent("""\
    execution_mode: sequential
    waves:
      - wave: 1
        mode: sequential
        tasks: [1.1]
        reason: "single task"
""")

MULTI_WAVE_YAML = dedent("""\
    execution_mode: mixed
    waves:
      - wave: 1
        mode: sequential
        tasks: [1.1]
        reason: "foundation"
      - wave: 2
        mode: parallel
        tasks: [2.1, 2.2]
        reason: "parallel work"
""")


def _make_single_doc_plan(tmp_path: Path, strategy_yaml: str = MINIMAL_STRATEGY_YAML) -> Path:
    """Create a minimal single-doc plan with ## Execution Strategy section."""
    plan_file = tmp_path / "my-feature.md"
    plan_file.write_text(
        "---\n"
        "status: ready\n"
        "execution_mode: sequential\n"
        "---\n"
        "\n"
        "# My Feature\n"
        "\n"
        "## Overview\n"
        "\n"
        "Test feature overview.\n"
        "\n"
        "## Execution Strategy\n"
        "\n"
        "```yaml\n"
        f"{strategy_yaml}"
        "```\n"
    )
    return plan_file


# ---------------------------------------------------------------------------
# AC4-EDGE: single-doc plan parses correctly
# ---------------------------------------------------------------------------


def test_AC4_EDGE_single_doc_plan_parses_correctly(tmp_path: Path) -> None:
    """Single-doc plan produces a valid ExecutionStrategy."""
    plan_file = _make_single_doc_plan(tmp_path)

    strategy = load_plan_strategy(str(plan_file))

    assert strategy is not None
    assert isinstance(strategy, ExecutionStrategy)
    assert len(strategy.waves) == 1
    assert strategy.waves[0].number == 1
    assert strategy.waves[0].mode == "sequential"
    assert strategy.waves[0].tasks == ["1.1"]


def test_single_doc_multi_wave_plan(tmp_path: Path) -> None:
    """Single-doc plan with multiple waves parses all waves correctly."""
    plan_file = _make_single_doc_plan(tmp_path, MULTI_WAVE_YAML)

    strategy = load_plan_strategy(str(plan_file))

    assert strategy is not None
    assert len(strategy.waves) == 2
    assert strategy.waves[1].tasks == ["2.1", "2.2"]


# ---------------------------------------------------------------------------
# AC4-FR: missing/unreadable plan file -> blocked result
# ---------------------------------------------------------------------------


def test_AC4_FR_missing_plan_file_returns_none(tmp_path: Path) -> None:
    """When the plan file doesn't exist, load_plan_strategy returns None (blocked)."""
    result = load_plan_strategy(str(tmp_path / "does-not-exist.md"))

    assert result is None


def test_AC4_FR_unreadable_plan_file_returns_none(tmp_path: Path, capsys: pytest.CaptureFixture) -> None:
    """When the plan file exists but is unreadable, load_plan_strategy returns None."""
    import os
    plan_file = tmp_path / "locked-plan.md"
    plan_file.write_text("# plan\n")
    # Make the file unreadable
    os.chmod(str(plan_file), 0o000)

    try:
        result = load_plan_strategy(str(plan_file))
        assert result is None
        _, err = capsys.readouterr()
        assert "plan_unreadable" in err or "blocked" in err.lower() or "unreadable" in err.lower()
    finally:
        os.chmod(str(plan_file), 0o644)


# ---------------------------------------------------------------------------
# Malformed Execution Strategy YAML in single-doc -> returns None + stderr
# ---------------------------------------------------------------------------


def test_malformed_execution_strategy_in_single_doc_returns_none(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    """Single-doc with invalid Execution Strategy YAML returns None and emits structured error."""
    plan_file = tmp_path / "malformed.md"
    plan_file.write_text(dedent("""\
        ---
        status: ready
        ---

        # Malformed Plan

        ## Execution Strategy

        ```yaml
        execution_mode: sequential
        waves:
          - wave: 1
            bad_indent:
              this: is: broken: yaml: [unclosed
        ```
    """))

    result = load_plan_strategy(str(plan_file))

    assert result is None
    _, err = capsys.readouterr()
    # Should emit something about the section name
    assert "Execution Strategy" in err or "malformed" in err.lower() or "yaml" in err.lower()


def test_missing_execution_strategy_section_in_single_doc(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    """Single-doc without ## Execution Strategy returns None with warning."""
    plan_file = tmp_path / "no-strategy.md"
    plan_file.write_text(dedent("""\
        ---
        status: ready
        ---

        # Plan without strategy

        ## Overview

        No execution strategy here.
    """))

    result = load_plan_strategy(str(plan_file))

    assert result is None
    _, err = capsys.readouterr()
    assert err  # must emit some diagnostic
