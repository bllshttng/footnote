"""Unit tests for fno.plan._locate - plan shape detection.

Covers:
- AC4-HP: folder plan with 00-INDEX.md resolves correctly
- AC4-EDGE: single .md file resolves correctly
- locate_plan raises PlanNotFound for missing path and dir without index
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.plan._locate import PlanNotFound, ResolvedPlan, locate_plan


# ---------------------------------------------------------------------------
# locate_plan: folder with 00-INDEX.md
# ---------------------------------------------------------------------------


def test_AC4_HP_folder_with_index_resolves_as_folder(tmp_path: Path) -> None:
    """locate_plan returns kind='folder' when the path is a directory with 00-INDEX.md."""
    # Given: a directory containing 00-INDEX.md
    plan_dir = tmp_path / "my-feature"
    plan_dir.mkdir()
    index = plan_dir / "00-INDEX.md"
    index.write_text("# plan\n")

    # When
    result = locate_plan(plan_dir)

    # Then
    assert result.kind == "folder"
    assert result.root_path == plan_dir
    assert result.index_path == index


def test_AC4_HP_folder_index_path_is_concrete(tmp_path: Path) -> None:
    """index_path on a folder result points at the actual 00-INDEX.md file."""
    plan_dir = tmp_path / "feat"
    plan_dir.mkdir()
    (plan_dir / "00-INDEX.md").write_text("# x\n")

    result = locate_plan(plan_dir)
    assert result.index_path is not None
    assert result.index_path.name == "00-INDEX.md"
    assert result.index_path.exists()


# ---------------------------------------------------------------------------
# locate_plan: single .md file
# ---------------------------------------------------------------------------


def test_AC4_EDGE_single_md_file_resolves_as_single(tmp_path: Path) -> None:
    """locate_plan returns kind='single' when given a path to a .md file."""
    # Given: a plan file at a concrete path
    plan_file = tmp_path / "my-feature.md"
    plan_file.write_text("---\nstatus: ready\n---\n# My Feature\n")

    # When
    result = locate_plan(plan_file)

    # Then
    assert result.kind == "single"
    assert result.root_path == plan_file
    assert result.index_path is None


def test_single_md_file_string_path_accepted(tmp_path: Path) -> None:
    """locate_plan accepts str input as well as Path."""
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# plan\n")

    result = locate_plan(str(plan_file))

    assert result.kind == "single"
    assert result.root_path == plan_file


# ---------------------------------------------------------------------------
# locate_plan: error cases
# ---------------------------------------------------------------------------


def test_AC4_FR_missing_path_raises_plan_not_found(tmp_path: Path) -> None:
    """locate_plan raises PlanNotFound when the path does not exist."""
    missing = tmp_path / "does-not-exist.md"

    with pytest.raises(PlanNotFound):
        locate_plan(missing)


def test_directory_without_index_raises_plan_not_found(tmp_path: Path) -> None:
    """locate_plan raises PlanNotFound when a directory lacks 00-INDEX.md."""
    empty_dir = tmp_path / "no-index"
    empty_dir.mkdir()

    with pytest.raises(PlanNotFound):
        locate_plan(empty_dir)


def test_plan_not_found_is_file_not_found_error(tmp_path: Path) -> None:
    """PlanNotFound is a subclass of FileNotFoundError."""
    missing = tmp_path / "ghost.md"

    with pytest.raises(FileNotFoundError):
        locate_plan(missing)


# ---------------------------------------------------------------------------
# ResolvedPlan is frozen / immutable
# ---------------------------------------------------------------------------


def test_resolved_plan_is_frozen(tmp_path: Path) -> None:
    """ResolvedPlan is a frozen dataclass - assignments raise FrozenInstanceError."""
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# plan\n")

    result = locate_plan(plan_file)

    with pytest.raises(Exception):
        result.kind = "folder"  # type: ignore[misc]
