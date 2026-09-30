"""Unit tests for the prod tripwire over fake roots (never the real ones)."""

from __future__ import annotations

from pathlib import Path

from tests.prod_tripwire import find_leaks, live_roots, snapshot


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

    # Root selection honors EXPLICIT sandbox declarations only, by canonical
    # path containment: a declared sandbox home is never watched, an
    # undeclared home with a matching layout (even a basename that extends a
    # declared one) stays watched, and an undeclared home keeps today's
    # behavior exactly.
    declared = tmp_path / "fno-test-sandbox-declared"
    (declared / "home" / ".fno").mkdir(parents=True)
    excludes = frozenset({str(declared.resolve())})
    assert live_roots(declared / "home", tmp_path, None, exclude=excludes) == []
    adjacent = tmp_path / "fno-test-sandbox-declaredx"
    (adjacent / "home" / ".fno").mkdir(parents=True)
    adjacent_roots = live_roots(adjacent / "home", tmp_path, None, exclude=excludes)
    assert [path for path, _ in adjacent_roots] == [adjacent / "home" / ".fno"]
    unlabeled = tmp_path / "real-home"
    (unlabeled / ".fno").mkdir(parents=True)
    unlabeled_roots = live_roots(unlabeled, tmp_path, None, exclude=excludes)
    assert [path for path, _ in unlabeled_roots] == [unlabeled / ".fno"]
    # An undeclared home with the sandbox's own layout stays watched: the name
    # alone is never the proof, the runner's declaration is.
    undeclared = tmp_path / "fno-test-sandbox-undeclared"
    (undeclared / "home" / ".fno").mkdir(parents=True)
    undeclared_roots = live_roots(undeclared / "home", tmp_path, None)
    assert [path for path, _ in undeclared_roots] == [undeclared / "home" / ".fno"]


def test_find_leaks_respects_depth(tmp_path: Path) -> None:
    root = tmp_path / "watched"
    (root / "deep").mkdir(parents=True)
    roots = [(root, 1)]
    before = snapshot(roots)
    (root / "deep" / "x-pytest-of-u-pytest-7.md").write_text("x")
    (root / "top-pytest-of-u-pytest-7.md").write_text("x")

    leaks = find_leaks(before, roots, {str(tmp_path / "pytest-of-u" / "pytest-7")})

    assert leaks == [root / "top-pytest-of-u-pytest-7.md"]
