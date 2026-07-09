"""Bank task runner: disposable worktree -> optional worker -> mechanical grade.

One run = one task executed once. ``--repeat K`` (pass^k) is K runs of the same
task, each in its own fresh disposable worktree (Invariant: a bank task never
executes in the user's working copy).

The worker step is injectable (``spawn``) so tests never spawn a real model and
never spend money. The default spawn routes through ``fno agents spawn
--substrate headless`` (the x-2c27 rule: never bare ``claude -p``; the substrate
path keeps provider rotation and the spawn cap in play).
"""
from __future__ import annotations

import os
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional

from fno.evals import history as _history
from fno.evals.bank import TaskSpec
from fno.evals.grading import GradeOutcome, grade


@dataclass(frozen=True)
class SpawnResult:
    ok: bool
    reason: str = ""


# spawn(prompt, workdir, timeout_s) -> SpawnResult
SpawnFn = Callable[[str, Path, int], SpawnResult]


@dataclass(frozen=True)
class RunResult:
    task_id: str
    tier: str
    passed: bool
    reason: str
    duration_s: float
    repeat_index: int


def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def _git_rev(repo_root: Path, ref: str) -> Optional[str]:
    try:
        proc = subprocess.run(
            ["git", "rev-parse", ref], cwd=str(repo_root),
            capture_output=True, text=True, timeout=10,
        )
        return proc.stdout.strip() if proc.returncode == 0 else None
    except Exception:  # noqa: BLE001 - a missing/unreadable ref is a graded failure, not a crash
        return None


def _default_spawn(
    prompt: str, workdir: Path, timeout_s: int, *, provider: Optional[str] = None
) -> SpawnResult:
    """Run the worker via ``fno agents spawn --substrate headless`` in *workdir*.

    A non-zero exit, a missing binary, or a timeout is a *graded failure*
    (SpawnResult.ok == False), never a crash of the sweep (AC3-ERR).
    """
    name = f"eval-{os.getpid()}-{int(time.time())}"
    cmd = [
        "fno", "agents", "spawn", name, prompt,
        "--substrate", "headless", "--cwd", str(workdir),
        "--timeout", str(timeout_s),
    ]
    if provider:
        cmd += ["--provider", provider]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout_s + 30)
    except FileNotFoundError:
        return SpawnResult(False, "spawn failed: `fno` binary not found")
    except subprocess.TimeoutExpired:
        return SpawnResult(False, f"spawn timed out after {timeout_s}s")
    except Exception as exc:  # noqa: BLE001 - any spawn error is a graded fail
        return SpawnResult(False, f"spawn error: {exc}")
    if proc.returncode != 0:
        tail = (proc.stderr or proc.stdout or "").strip().splitlines()[-1:] or [""]
        return SpawnResult(False, f"spawn exit {proc.returncode}: {tail[0]}")
    return SpawnResult(True)


def _make_disposable_worktree(repo_root: Path, ref: str, tag: str) -> Path:
    from fno.worktree_paths import worktree_base

    base = worktree_base() / "evals"
    base.mkdir(parents=True, exist_ok=True)
    path = base / f"{tag}-{os.getpid()}-{int(time.time()*1000)}"
    subprocess.run(
        ["git", "worktree", "add", "--detach", str(path), ref],
        cwd=str(repo_root), capture_output=True, text=True, check=True,
    )
    return path


def _remove_worktree(repo_root: Path, path: Path) -> None:
    # Best-effort: a failed removal must never mask the run's verdict. The next
    # `fno evals run` sweeps orphans (see sweep_orphans).
    subprocess.run(
        ["git", "worktree", "remove", "--force", str(path)],
        cwd=str(repo_root), capture_output=True, text=True,
    )
    subprocess.run(["git", "worktree", "prune"], cwd=str(repo_root),
                   capture_output=True, text=True)


def sweep_orphans(repo_root: Path) -> int:
    """Prune any leftover eval worktrees from a prior crashed run.

    Returns the number of eval worktrees removed. Best-effort; errors are
    swallowed (a sweep failure must not block a fresh run).
    """
    from fno.worktree_paths import worktree_base

    removed = 0
    try:
        proc = subprocess.run(
            ["git", "worktree", "list", "--porcelain"],
            cwd=str(repo_root), capture_output=True, text=True, timeout=10,
        )
    except Exception:  # noqa: BLE001
        return 0
    evals_base = str(worktree_base() / "evals")
    for line in proc.stdout.splitlines():
        if line.startswith("worktree ") and evals_base in line:
            wt = line[len("worktree "):].strip()
            _remove_worktree(repo_root, Path(wt))
            removed += 1
    return removed


def run_task(
    task: TaskSpec,
    *,
    repeat: int = 1,
    repo_root: Path,
    spawn: Optional[SpawnFn] = None,
    history_path: Optional[Path] = None,
    worker_provider: Optional[str] = None,
) -> list[RunResult]:
    """Run *task* ``repeat`` times, appending one history row per run.

    Each run: fresh disposable worktree at ``task.repo_fixture`` -> optional
    worker (skipped for a grade-only task) -> mechanical grade -> history row ->
    worktree removed (Invariant: removed after grading). A worker-spawn failure
    is recorded as a graded fail and the remaining repeats still run (AC3-ERR).
    """
    # When no spawn is injected, bind the worker provider into the default spawn
    # so --provider actually routes the headless worker (not just logged).
    spawn_fn = spawn or (
        lambda p, w, t: _default_spawn(p, w, t, provider=worker_provider)
    )
    if history_path is None:
        from fno import paths as _paths
        history_path = _paths.evals_history()
    bank_rev = _git_rev(repo_root, task.repo_fixture)
    timeout_s = max(1, task.timeout_minutes * 60)
    results: list[RunResult] = []

    for i in range(repeat):
        started = time.monotonic()
        reason = ""
        outcome: Optional[GradeOutcome] = None
        workdir: Optional[Path] = None
        try:
            workdir = _make_disposable_worktree(repo_root, task.repo_fixture, task.id)
        except subprocess.CalledProcessError as exc:
            # Fixture checkout failed: graded fail with a drift hint, not a crash.
            reason = f"fixture checkout failed ({task.repo_fixture}); fixture drift? {exc.stderr or ''}".strip()

        if workdir is not None:
            if task.prompt:
                spawn_res = spawn_fn(task.prompt, workdir, timeout_s)
                if not spawn_res.ok:
                    reason = spawn_res.reason
            if not reason:
                outcome = grade(task, workdir)
                if not outcome.passed:
                    reason = outcome.reason
            _remove_worktree(repo_root, workdir)

        duration = round(time.monotonic() - started, 3)
        passed = outcome is not None and outcome.passed
        results.append(RunResult(
            task_id=task.id, tier=task.tier, passed=passed,
            reason="" if passed else reason, duration_s=duration, repeat_index=i,
        ))
        _history.append_row(history_path, {
            "ts": _now_iso(),
            "task_id": task.id,
            "tier": task.tier,
            "pass": passed,
            "reason": "" if passed else reason,
            "duration_s": duration,
            "repeat_index": i,
            "bank_rev": bank_rev,
            "worker_provider": worker_provider,
        })

    return results
