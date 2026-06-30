"""Tests for the plan-vs-reality reconcile delta (x-a7be, change C)."""
from __future__ import annotations

from pathlib import Path

from fno.plan.reconcile import ReconcileDelta, reconcile_plan


def _write(p: Path, body: str) -> Path:
    p.write_text(body, encoding="utf-8")
    return p


def test_present_and_stale_paths(tmp_path: Path) -> None:
    repo = tmp_path
    (repo / "cli" / "src" / "fno").mkdir(parents=True)
    _write(repo / "cli" / "src" / "fno" / "live.py", "x = 1\n")
    plan = _write(
        repo / "plan.md",
        "## Files to Modify\n"
        "| `cli/src/fno/live.py` | edit |\n"
        "| `cli/src/fno/gone.py` | new |\n",
    )
    delta = reconcile_plan(plan, repo)
    assert delta.present == 1
    assert delta.stale == 1
    summary = delta.summary()
    assert "1 present" in summary and "1 stale-reference" in summary


def test_dotted_non_paths_excluded(tmp_path: Path) -> None:
    # `config.target.blast` is a config key, not a file path (no slash) -> ignored.
    plan = _write(
        tmp_path / "plan.md",
        "Gate on `config.target.blast` and the `fno target status` verb.\n",
    )
    delta = reconcile_plan(plan, tmp_path)
    assert delta.present == 0 and delta.stale == 0
    assert "none" in delta.summary()


def test_unreadable_plan_degrades(tmp_path: Path) -> None:
    delta = reconcile_plan(tmp_path / "does-not-exist.md", tmp_path)
    assert delta.note is not None
    assert "unknown" in delta.summary()


def test_index_fragment_stripped(tmp_path: Path) -> None:
    # A `path/to/plan#fragment` index pointer must read the real file.
    plan = _write(tmp_path / "plan.md", "touches `a/b/c.py`\n")
    delta = reconcile_plan(f"{plan}#some-anchor", tmp_path)
    assert delta.stale == 1  # a/b/c.py does not exist under tmp_path


def test_dedup_repeated_path(tmp_path: Path) -> None:
    plan = _write(
        tmp_path / "plan.md",
        "`x/y.py` first mention, then `x/y.py` again.\n",
    )
    delta = reconcile_plan(plan, tmp_path)
    assert delta.stale == 1
    assert len(delta.paths) == 1


def test_absolute_path_does_not_bypass_repo_root(tmp_path: Path) -> None:
    # An absolute path that exists on the host but is NOT under repo_root must be
    # stale, not present: a bare `repo_root / "/abs/x.py"` would resolve to the
    # host file (Path discards repo_root when joined with an absolute path).
    outside = tmp_path / "outside.py"  # has a slash + .py ext -> matches the regex
    outside.write_text("x = 1\n", encoding="utf-8")
    repo = tmp_path / "repo"
    repo.mkdir()
    plan = _write(repo / "plan.md", f"touches `{outside.as_posix()}`\n")
    delta = reconcile_plan(plan, repo)
    assert delta.stale == 1 and delta.present == 0


def test_folder_plan_reads_index(tmp_path: Path) -> None:
    # A folder plan (dir + 00-INDEX.md) must reconcile its index, not degrade
    # to "Is a directory".
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "live.py").write_text("x = 1\n", encoding="utf-8")
    folder = tmp_path / "myplan"
    folder.mkdir()
    (folder / "00-INDEX.md").write_text("touches `src/live.py`\n", encoding="utf-8")
    delta = reconcile_plan(folder, tmp_path)
    assert delta.note is None
    assert delta.present == 1


def test_self_check_runs() -> None:
    # The module ships a runnable assert-based self-check.
    from fno.plan import reconcile

    reconcile._self_check()


def test_is_dataclass_immutable() -> None:
    d = ReconcileDelta(present=0, stale=0, paths=())
    assert d.summary().startswith("none")
