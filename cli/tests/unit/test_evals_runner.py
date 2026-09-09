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
import fno.evals.runner as _runner
from fno.evals.runner import SpawnResult, evals_enabled, run_task
from fno.route_resolve import InventoryRow


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
    assert rows[0]["variant"] == "baseline"  # default run records the round
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


def test_config_disabled_skips_the_default_spawn(tmp_path: Path, monkeypatch) -> None:
    """x-aaaf wave 2 (AC4-HP): config.evals.enabled=false stops the REAL
    default spawn (no `spawn=` injected) - it must never reach _default_spawn."""
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"
    monkeypatch.setattr(_runner, "evals_enabled", lambda: False)

    def _boom(*a, **kw):
        raise AssertionError("_default_spawn must not run when the gate is off")

    monkeypatch.setattr(_runner, "_default_spawn", _boom)

    task = _task(prompt="x", grade=[GradeCheck("file-exists", path="made.txt")])
    results = run_task(task, repeat=1, repo_root=root, history_path=hp)
    assert not results[0].passed
    assert "config.evals.enabled is false" in results[0].reason


def test_config_disabled_never_blocks_an_injected_spawn(tmp_path: Path, monkeypatch) -> None:
    """The gate only bites the real default spawn; a caller-injected spawn_fn
    (tests, or an explicit invocation) is unaffected."""
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"
    monkeypatch.setattr(_runner, "evals_enabled", lambda: False)

    def spawn(prompt: str, workdir: Path, timeout_s: int) -> SpawnResult:
        (workdir / "made.txt").write_text("ok\n", encoding="utf-8")
        return SpawnResult(True)

    task = _task(prompt="x", grade=[GradeCheck("file-exists", path="made.txt")])
    results = run_task(task, repeat=1, repo_root=root, history_path=hp, spawn=spawn)
    assert results[0].passed


def test_evals_enabled_defaults_true_matching_prior_ungated_behavior(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "nonexistent.yaml"))

    assert evals_enabled() is True


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
# variant axis: a scored round names baseline or v<N> and checks out its ref
# --------------------------------------------------------------------------- #

def _branch(root: Path, name: str, filename: str) -> None:
    def g(*args: str) -> None:
        subprocess.run(["git", *args], cwd=str(root), check=True, capture_output=True)
    g("checkout", "-b", name)
    (root / filename).write_text("x\n", encoding="utf-8")
    g("add", "-A")
    g("commit", "-qm", filename)
    g("checkout", "-")


def _git_sha(root: Path, ref: str) -> str:
    proc = subprocess.run(["git", "rev-parse", ref], cwd=str(root),
                          capture_output=True, text=True, check=True)
    return proc.stdout.strip()


def test_variant_run_checks_out_variant_ref_and_records_row(tmp_path: Path) -> None:
    root = _git_repo(tmp_path)
    _branch(root, "v1-work", "extra.txt")
    hp = tmp_path / "hist.jsonl"
    task = _task(grade=[GradeCheck("file-exists", path="extra.txt")])
    before = _worktree_count(root)

    results = run_task(task, repeat=1, repo_root=root, history_path=hp,
                       spawn=_never_called_spawn, variant="v1", variant_ref="v1-work")
    assert results[0].passed
    assert results[0].variant == "v1"
    rows = list(_history.iter_rows(hp))
    assert rows[0]["variant"] == "v1"
    assert rows[0]["bank_rev"] == _git_sha(root, "v1-work")
    assert _worktree_count(root) == before  # worktree removed after grading


def test_variant_bad_name_refuses_before_any_worktree(tmp_path: Path) -> None:
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"
    before = _worktree_count(root)

    with pytest.raises(ValueError, match=r"baseline\|v<N>"):
        run_task(_task(), repeat=1, repo_root=root, history_path=hp,
                 spawn=_never_called_spawn, variant="round2")
    with pytest.raises(ValueError, match="baseline"):
        run_task(_task(), repeat=1, repo_root=root, history_path=hp,
                 spawn=_never_called_spawn, variant="baseline", variant_ref="x")
    assert len(list(_history.iter_rows(hp))) == 0  # nothing recorded
    assert _worktree_count(root) == before          # no worktree made


def test_variant_bad_ref_is_graded_fail_with_ref_name(tmp_path: Path) -> None:
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"
    results = run_task(_task(), repeat=1, repo_root=root, history_path=hp,
                       spawn=_never_called_spawn, variant="v1", variant_ref="no-such-ref")
    assert not results[0].passed
    assert "no-such-ref" in results[0].reason


# --------------------------------------------------------------------------- #
# small helpers
# --------------------------------------------------------------------------- #

def _never_called_spawn(prompt: str, workdir: Path, timeout_s: int) -> SpawnResult:
    raise AssertionError("grade-only task must not spawn a worker")


# --------------------------------------------------------------------------- #
# lane requested/observed evidence - AC1-HP, AC1-EDGE, AC1-ERR
# --------------------------------------------------------------------------- #

_LANE = InventoryRow(name="astra-high", harness="codex", model="gpt-6-astra", effort="high")


def test_lane_hp_records_requested_and_observed_configuration(tmp_path: Path) -> None:
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"

    def spawn(prompt: str, workdir: Path, timeout_s: int) -> SpawnResult:
        (workdir / "made.txt").write_text("ok\n", encoding="utf-8")
        return SpawnResult(True, worker_name="eval-worker-1")

    def observe(name: str):
        assert name == "eval-worker-1"
        return {"harness": "codex", "model": "gpt-6-astra", "model_basis": "verified",
                "effort": "high", "harness_session_id": "sess-1"}

    task = _task(prompt="do the thing", grade=[GradeCheck("file-exists", path="made.txt")])
    run_task(task, repeat=1, repo_root=root, history_path=hp, spawn=spawn,
             lane=_LANE, experiment_id="cohort-a", observe=observe)
    row = list(_history.iter_rows(hp))[0]
    assert row["requested_lane"] == "astra-high"
    assert row["requested_harness"] == "codex"
    assert row["requested_model"] == "gpt-6-astra"
    assert row["observed_harness"] == "codex"
    assert row["observed_model"] == "gpt-6-astra"
    assert row["observed_session_id"] == "sess-1"
    assert row["lane_status"] == "ok"
    assert row["substituted"] is False
    assert row["experiment_id"] == "cohort-a"


def test_lane_edge_substitution_is_labeled_and_excluded(tmp_path: Path) -> None:
    """AC1-EDGE: capacity serves a different harness -> substituted, not folded
    into the requested lane's cohort."""
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"

    def spawn(prompt: str, workdir: Path, timeout_s: int) -> SpawnResult:
        (workdir / "made.txt").write_text("ok\n", encoding="utf-8")
        return SpawnResult(True, worker_name="eval-worker-2")

    def observe(name: str):
        return {"harness": "claude", "model": "claude-sonnet-5", "effort": "medium"}

    task = _task(prompt="do the thing", grade=[GradeCheck("file-exists", path="made.txt")])
    run_task(task, repeat=1, repo_root=root, history_path=hp, spawn=spawn,
             lane=_LANE, observe=observe)
    row = list(_history.iter_rows(hp))[0]
    assert row["substituted"] is True
    assert row["lane_status"] == "substituted"
    assert row["requested_harness"] == "codex"
    assert row["observed_harness"] == "claude"


def test_lane_err_unavailable_never_grades_a_substitute_as_requested(tmp_path: Path) -> None:
    """AC1-ERR: a spawn refusal records unavailable with no observed model -
    the run is graded a fail, never a pass under the requested lane."""
    root = _git_repo(tmp_path)
    hp = tmp_path / "hist.jsonl"

    def spawn(prompt: str, workdir: Path, timeout_s: int) -> SpawnResult:
        return SpawnResult(False, "spawn exit 2: account lacks model")

    def _boom(name: str):
        raise AssertionError("no worker ran; observe must not be called")

    task = _task(prompt="do the thing", grade=[GradeCheck("file-exists", path="made.txt")])
    results = run_task(task, repeat=1, repo_root=root, history_path=hp, spawn=spawn,
                       lane=_LANE, observe=_boom)
    assert not results[0].passed
    row = list(_history.iter_rows(hp))[0]
    assert row["lane_status"] == "unavailable"
    assert "observed_model" not in row
    assert row["requested_model"] == "gpt-6-astra"


def _worktree_count(root: Path) -> int:
    proc = subprocess.run(["git", "worktree", "list", "--porcelain"],
                          cwd=str(root), capture_output=True, text=True)
    return proc.stdout.count("worktree ")
