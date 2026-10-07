"""Tests for fno.state.v2_paths."""

from __future__ import annotations

from pathlib import Path

from fno.state.v2_paths import v1_state_path, v2_state_path


def test_v2_paths_are_isolated_under_v2_directory(tmp_path: Path) -> None:
    assert v2_state_path(tmp_path) == tmp_path / ".fno" / "v2" / "target-state.md"
    assert v1_state_path(tmp_path) == tmp_path / ".fno" / "target-state.md"
