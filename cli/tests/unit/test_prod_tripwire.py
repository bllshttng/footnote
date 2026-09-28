"""Unit tests for the prod tripwire over fake roots (never the real ones)."""

from __future__ import annotations

from pathlib import Path

from tests.prod_tripwire import find_leaks, snapshot


def test_find_leaks_names_marked_entries_only(tmp_path: Path) -> None:
    root = tmp_path / "watched"
    root.mkdir()
    roots = [(root, 2)]
    basetemp = tmp_path / "pytest-of-u" / "pytest-7"
    markers = {str(basetemp), str(tmp_path / "sandbox")}
    before = snapshot(roots)

    (root / "plan.md").write_text(f"source_doc: {basetemp}/x/plan.md")
    (root / "x-pytest-of-u-pytest-7-y").mkdir()
    (root / "x-pytest-of-u-pytest-8-y").mkdir()
    (root / "stray.txt").write_text("nothing here")
    pruned = root / "worktrees"
    pruned.mkdir()
    (pruned / "x-pytest-of-u-pytest-7-y.md").write_text("x")
    (root / "fno-probe-canary-abc").write_text("x")

    leaks = find_leaks(before, roots, markers)

    assert leaks == [root / "plan.md", root / "x-pytest-of-u-pytest-7-y"]


def test_find_leaks_respects_depth(tmp_path: Path) -> None:
    root = tmp_path / "watched"
    (root / "deep").mkdir(parents=True)
    roots = [(root, 1)]
    before = snapshot(roots)
    (root / "deep" / "x-pytest-of-u-pytest-7.md").write_text("x")
    (root / "top-pytest-of-u-pytest-7.md").write_text("x")

    leaks = find_leaks(before, roots, {str(tmp_path / "pytest-of-u" / "pytest-7")})

    assert leaks == [root / "top-pytest-of-u-pytest-7.md"]
