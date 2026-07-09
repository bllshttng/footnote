"""Runner + grading + history (US2): AC1-HP, AC3-ERR, AC5-FR.

Every test injects a fake spawn - no real model, no money. A real git repo is
built in tmp so the disposable-worktree lifecycle is exercised for real.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno.evals import history as _history
from fno.evals.bank import GradeCheck, TaskSpec
from fno.evals.grading import grade
from fno.evals.runner import SpawnResult, run_task


# --------------------------------------------------------------------------- #
# helpers
# --------------------------------------------------------------------------- #

@pytest.fixture(autouse=True)
def _isolate_worktree_base(tmp_path, monkeypatch):
    """Keep disposable eval worktrees inside tmp, never the real worktree base."""
    base = tmp_path / "wtbase"
    base.mkdir()
    monkeypatch.setattr("fno.worktree_paths.worktree_base", lambda: base)


def _git_repo(tmp_path: Path) -> Path:
    root = tmp_path / "repo"
    root.mkdir()
    def g(*args: str) -> None:
        subprocess.run(["git", *args], cwd=str(root), check=True, capture_output=True)
    g("init", "-q")
    g("config", "user.email", "t@t.t")
    g("config", "user.name", "t")
    (root / "seed.txt").write_text("hello world\n", encoding="utf-8")
    g("add", "-A")
    g("commit", "-qm", "seed")
    return root


def _task(**kw) -> TaskSpec:
    base = dict(id="t", tier="regression", grade=[GradeCheck("exit", command="true")])
    base.update(kw)
    return TaskSpec(**base)  # type: ignore[arg-type]


# --------------------------------------------------------------------------- #
# grading (pure, no worktree)
# --------------------------------------------------------------------------- #

def test_grade_all_kinds(tmp_path: Path) -> None:
    (tmp_path / "a.txt").write_text("PASS here\n", encoding="utf-8")
    task = _task(grade=[
        GradeCheck("exit", command="true"),
        GradeCheck("file-exists", path="a.txt"),
        GradeCheck("grep", path="a.txt", pattern="PASS"),
    ])
    out = grade(task, tmp_path)
    assert out.passed
    assert out.reason == ""


def test_grade_fail_reports_first_reason(tmp_path: Path) -> None:
    task = _task(grade=[
        GradeCheck("exit", command="true"),
        GradeCheck("file-exists", path="missing.txt"),
    ])
    out = grade(task, tmp_path)
    assert not out.passed
    assert "missing.txt" in out.reason


def test_grade_exit_nonzero_fails(tmp_path: Path) -> None:
    out = grade(_task(grade=[GradeCheck("exit", command="exit 3", expect=0)]), tmp_path)
    assert not out.passed


# --------------------------------------------------------------------------- #
# history (append-only, tolerant read) - AC5-FR
# --------------------------------------------------------------------------- #

def test_history_append_and_tolerant_read(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {"task_id": "a", "pass": True})
    _history.append_row(hp, {"task_id": "b", "pass": False})
    rows = [r for _, r in _history.iter_rows_tolerant(hp)]
    assert [r["task_id"] for r in rows] == ["a", "b"]


def test_history_partial_final_line_tolerated(tmp_path: Path) -> None:
    hp = tmp_path / "h.jsonl"
    _history.append_row(hp, {"task_id": "a", "pass": True})
    # Simulate an interrupted append: a truncated JSON fragment with no newline.
    with hp.open("a", encoding="utf-8") as fh:
        fh.write('{"task_id": "b", "pa')
    with pytest.warns(UserWarning, match="malformed JSON"):
        rows = [r for _, r in _history.iter_rows_tolerant(hp)]
    assert [r["task_id"] for r in rows] == ["a"]  # task 1 survives, no crash


# --------------------------------------------------------------------------- #
# runner - AC1-HP
# --------------------------------------------------------------------------- #

def test_run_grade_only_task_appends_history_and_removes_worktree(tmp_path: Path) -> None:
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"
    task = _task(grade=[GradeCheck("file-exists", path="seed.txt")])  # seed exists in fixture

    before = _worktree_count(root)
    results = run_task(task, repeat=1, repo_root=root, history_path=hp,
                       spawn=_never_called_spawn)
    assert results[0].passed
    rows = list(_history.iter_rows(hp))
    assert len(rows) == 1
    assert rows[0]["pass"] is True and rows[0]["tier"] == "regression"
    assert rows[0]["bank_rev"]  # HEAD sha recorded
    assert _worktree_count(root) == before  # worktree removed after grading


def test_run_worker_task_invokes_spawn(tmp_path: Path) -> None:
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"
    calls: list[str] = []

    def spawn(prompt: str, workdir: Path, timeout_s: int) -> SpawnResult:
        calls.append(prompt)
        (workdir / "made.txt").write_text("ok\n", encoding="utf-8")
        return SpawnResult(True)

    task = _task(prompt="do the thing", grade=[GradeCheck("file-exists", path="made.txt")])
    results = run_task(task, repeat=1, repo_root=root, history_path=hp, spawn=spawn)
    assert calls == ["do the thing"]
    assert results[0].passed


# AC3-ERR: spawn failure is a graded fail, remaining repeats still run.
def test_spawn_failure_is_graded_fail_not_crash(tmp_path: Path) -> None:
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"

    def spawn(prompt: str, workdir: Path, timeout_s: int) -> SpawnResult:
        return SpawnResult(False, "spawn exit 1: provider down")

    task = _task(prompt="x", grade=[GradeCheck("file-exists", path="made.txt")])
    results = run_task(task, repeat=3, repo_root=root, history_path=hp, spawn=spawn)
    assert len(results) == 3
    assert all(not r.passed for r in results)
    assert all("provider down" in r.reason for r in results)
    assert len(list(_history.iter_rows(hp))) == 3  # every run recorded


def test_repeat_k_runs_k_times(tmp_path: Path) -> None:
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"
    task = _task(grade=[GradeCheck("file-exists", path="seed.txt")])
    results = run_task(task, repeat=4, repo_root=root, history_path=hp,
                       spawn=_never_called_spawn)
    assert len(results) == 4
    assert all(r.passed for r in results)
    assert [r.repeat_index for r in results] == [0, 1, 2, 3]


# --------------------------------------------------------------------------- #
# small helpers
# --------------------------------------------------------------------------- #

def _never_called_spawn(prompt: str, workdir: Path, timeout_s: int) -> SpawnResult:
    raise AssertionError("grade-only task must not spawn a worker")


def _worktree_count(root: Path) -> int:
    proc = subprocess.run(["git", "worktree", "list", "--porcelain"],
                          cwd=str(root), capture_output=True, text=True)
    return proc.stdout.count("worktree ")
