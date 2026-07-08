"""Tests for fno.plan._folder_audit (x-8c05 precondition verb)."""
from __future__ import annotations

from pathlib import Path

from fno.plan._folder_audit import scan


def _make_folder_plan(plans_root: Path, name: str) -> Path:
    folder = plans_root / name
    folder.mkdir(parents=True)
    (folder / "00-INDEX.md").write_text("---\nstatus: ready\n---\n# plan\n", encoding="utf-8")
    return folder


def test_non_terminal_owner_counted(tmp_path: Path) -> None:
    _make_folder_plan(tmp_path, "live-folder")
    entries = [{"id": "ab-1", "_status": "ready", "plan_path": "internal/fno/plans/live-folder"}]
    owners = scan(tmp_path, entries)
    assert owners is not None
    assert [o.node_id for o in owners] == ["ab-1"]


def test_terminal_owner_not_counted(tmp_path: Path) -> None:
    # Frontmatter says "ready" (stale) but the owning node is done - must not count.
    _make_folder_plan(tmp_path, "done-folder")
    entries = [{"id": "ab-2", "_status": "done", "plan_path": "internal/fno/plans/done-folder"}]
    owners = scan(tmp_path, entries)
    assert owners == []


def test_folder_with_no_owning_node_not_counted(tmp_path: Path) -> None:
    _make_folder_plan(tmp_path, "orphan-folder")
    entries = [{"id": "ab-3", "_status": "ready", "plan_path": "internal/fno/plans/some-other-doc.md"}]
    owners = scan(tmp_path, entries)
    assert owners == []


def test_basename_join_ignores_absolute_root_mismatch(tmp_path: Path) -> None:
    # Graph plan_path is under a different absolute root than plans_root
    # (abilities-vs-fno rename); basename join must still match.
    _make_folder_plan(tmp_path, "renamed-root-folder")
    entries = [
        {"id": "ab-4", "_status": "ready", "plan_path": "/Users/other/abilities/plans/renamed-root-folder"}
    ]
    owners = scan(tmp_path, entries)
    assert owners is not None
    assert [o.node_id for o in owners] == ["ab-4"]


def test_unscannable_plans_root_returns_none(tmp_path: Path) -> None:
    missing = tmp_path / "does-not-exist"
    owners = scan(missing, [{"id": "ab-5", "_status": "ready", "plan_path": "x"}])
    assert owners is None
