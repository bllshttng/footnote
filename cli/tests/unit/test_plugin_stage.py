"""Tests for the filtered plugin stage (x-7ca7 task 3.1)."""
from __future__ import annotations

import subprocess
from pathlib import Path

from fno.setup.plugin_stage import _stage_file_list, build_stage


def _init_repo(root: Path) -> None:
    root.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(["git", "config", "user.email", "t@localhost"], cwd=root, check=True)
    subprocess.run(["git", "config", "user.name", "t"], cwd=root, check=True)


def test_stage_excludes_ignored_bulk(tmp_path: Path) -> None:
    root = tmp_path / "repo"
    _init_repo(root)
    (root / ".gitignore").write_text("target/\n.claude/worktrees/\n")
    (root / ".claude-plugin").mkdir()
    (root / ".claude-plugin" / "plugin.json").write_text("{}")
    (root / "target" / "debug" / "deps").mkdir(parents=True)
    (root / "target" / "debug" / "deps" / "fno-abc").write_bytes(b"\0")
    (root / ".claude" / "worktrees" / "x-1" / "crates" / "f" / "target").mkdir(parents=True)
    (root / ".claude" / "worktrees" / "x-1" / "notes.md").write_text("x")
    subprocess.run(["git", "add", "."], cwd=root, check=True)
    subprocess.run(["git", "commit", "-qm", "init"], cwd=root, check=True)

    listed = _stage_file_list(root)
    assert ".claude-plugin/plugin.json" in listed
    assert not any(p.startswith("target/") for p in listed)
    assert not any(p.startswith(".claude/worktrees/") for p in listed)

    dest = build_stage(root)
    assert (dest / ".claude-plugin" / "plugin.json").is_file()
    assert not (dest / "target").exists()
    assert not (dest / ".claude" / "worktrees").exists()


def test_stage_replaces_the_previous_build_wholesale(tmp_path: Path) -> None:
    root = tmp_path / "repo"
    _init_repo(root)
    (root / "a.md").write_text("1")
    subprocess.run(["git", "add", "."], cwd=root, check=True)
    subprocess.run(["git", "commit", "-qm", "one"], cwd=root, check=True)
    dest = build_stage(root)
    assert (dest / "a.md").is_file()

    (root / "a.md").unlink()
    (root / "b.md").write_text("2")
    subprocess.run(["git", "add", "."], cwd=root, check=True)
    subprocess.run(["git", "commit", "-qm", "two"], cwd=root, check=True)
    dest = build_stage(root)
    assert not (dest / "a.md").exists(), "a deleted file must not survive as stale stage"
    assert (dest / "b.md").is_file()
