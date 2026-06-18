"""Tests for fno.state.v2_paths.

Covers AC1-HP, AC1-ERR, AC1-EDGE from plan phase 04.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from fno.state.v2_paths import (
    detect_v1_conflict,
    ensure_v2_layout,
    v1_state_path,
    v2_artifacts_dir,
    v2_root,
    v2_state_path,
)


def test_v2_paths_are_isolated_under_v2_directory(tmp_path: Path) -> None:
    assert v2_root(tmp_path) == tmp_path / ".fno" / "v2"
    assert v2_state_path(tmp_path) == tmp_path / ".fno" / "v2" / "target-state.md"
    assert (
        v2_artifacts_dir(tmp_path)
        == tmp_path / ".fno" / "v2" / "artifacts"
    )
    assert v1_state_path(tmp_path) == tmp_path / ".fno" / "target-state.md"


def test_no_v1_state_no_conflict(tmp_path: Path) -> None:
    assert detect_v1_conflict(tmp_path) is None


def test_malformed_v1_state_no_conflict(tmp_path: Path) -> None:
    state = v1_state_path(tmp_path)
    state.parent.mkdir(parents=True, exist_ok=True)
    state.write_text("not a frontmatter file", encoding="utf-8")
    assert detect_v1_conflict(tmp_path) is None


def _write_v1_state(tmp_path: Path, pid: int) -> None:
    state = v1_state_path(tmp_path)
    state.parent.mkdir(parents=True, exist_ok=True)
    state.write_text(
        f"---\n"
        f"status: IN_PROGRESS\n"
        f"session_id: s-1\n"
        f"owner_pid: {pid}\n"
        f"---\n# body\n",
        encoding="utf-8",
    )


def test_dead_pid_no_conflict(tmp_path: Path) -> None:
    """AC1-EDGE: v1 state exists but owner_pid is dead -> no conflict."""
    # PID 1 is init/launchd - always alive. Use a known-dead high pid.
    _write_v1_state(tmp_path, pid=999999)
    assert detect_v1_conflict(tmp_path) is None


def test_live_pid_returns_conflict(tmp_path: Path) -> None:
    """Use the current test process - guaranteed alive."""
    _write_v1_state(tmp_path, pid=os.getpid())
    reason = detect_v1_conflict(tmp_path)
    assert reason is not None
    assert "s-1" in reason
    assert str(os.getpid()) in reason


def test_missing_owner_pid_no_conflict(tmp_path: Path) -> None:
    state = v1_state_path(tmp_path)
    state.parent.mkdir(parents=True, exist_ok=True)
    state.write_text(
        "---\n"
        "status: IN_PROGRESS\n"
        "session_id: s-1\n"
        "---\n",
        encoding="utf-8",
    )
    assert detect_v1_conflict(tmp_path) is None


def test_negative_pid_no_conflict(tmp_path: Path) -> None:
    _write_v1_state(tmp_path, pid=-1)
    assert detect_v1_conflict(tmp_path) is None


def test_ensure_v2_layout_creates_artifacts_dir(tmp_path: Path) -> None:
    ensure_v2_layout(tmp_path)
    assert v2_artifacts_dir(tmp_path).is_dir()
    # Idempotent
    ensure_v2_layout(tmp_path)
    assert v2_artifacts_dir(tmp_path).is_dir()
