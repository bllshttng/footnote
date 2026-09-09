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
import re
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Optional

from fno.evals import history as _history
from fno.evals.bank import TaskSpec
from fno.evals.grading import GradeOutcome, grade


@dataclass(frozen=True)
class SpawnResult:
    ok: bool
    reason: str = ""
    worker_name: str = ""  # set on the real spawn; read back for observe()


# spawn(prompt, workdir, timeout_s) -> SpawnResult
SpawnFn = Callable[[str, Path, int], SpawnResult]


def _observe_worker(name: str) -> Optional[dict]:
    """Real worker identity from the registry row the spawn wrote; unreadable/missing reads as unobserved."""
    try:
        from fno.agents.registry import load_registry
        for entry in load_registry():
            if entry.name == name:
                return {"harness": entry.harness, "model": entry.model,
                        "model_basis": entry.model_basis, "effort": entry.effort,
                        "harness_session_id": entry.harness_session_id}
    except Exception:  # noqa: BLE001
        return None
    return None


def _lane_evidence(lane: Optional[Any], observed: Optional[dict], *, attempted: bool = True) -> dict[str, object]:
    """Requested vs. observed config for one run; a harness/model mismatch is ``substituted``.
    A grade-only task never attempts a worker, so ``attempted=False`` reads as
    ``not-applicable`` rather than a false ``unavailable`` capacity signal."""
    if lane is None:
        return {}
    fields: dict[str, object] = {
        "requested_lane": lane.name, "requested_harness": lane.harness,
        "requested_model": lane.model, "requested_effort": lane.effort,
    }
    if observed is None:
        fields["lane_status"] = "not-applicable" if not attempted else "unavailable"
        return fields
    fields.update(
        observed_harness=observed.get("harness"), observed_model=observed.get("model"),
        observed_model_basis=observed.get("model_basis"), observed_effort=observed.get("effort"),
        observed_session_id=observed.get("harness_session_id"),
    )
    substituted = (bool(lane.harness) and observed.get("harness") != lane.harness) or (
        bool(lane.model) and observed.get("model") != lane.model)
    fields["substituted"] = substituted
    fields["lane_status"] = "substituted" if substituted else "ok"
    return fields


@dataclass(frozen=True)
class RunResult:
    task_id: str
    tier: str
    passed: bool
    reason: str
    duration_s: float
    repeat_index: int
    variant: str = "baseline"


VARIANT_RE = re.compile(r"^(baseline|v[1-9]\d*)$")

#: The implicit round of rows written before the variant axis existed.
BASELINE = "baseline"


def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def evals_enabled() -> bool:
    """Resolve whether the headless grading-worker spawn is armed (x-aaaf wave 2).

    ``config.evals.enabled`` defaults True (matches the spawner's prior,
    ungated behavior). A malformed value degrades to True (never opt-in), but
    a config that fails to load at all degrades to False - the global
    invariant that an unreadable config resolves every gate to off, never on.

    Also stops when ``config.autonomy.enabled`` (the wave-3 master panic
    switch) is off, checked first.
    """
    from fno.config import autonomy_master_enabled

    if not autonomy_master_enabled():
        return False
    try:
        from fno.config import load_settings

        return bool(load_settings().evals.enabled)
    except Exception:  # noqa: BLE001 - fail-safe to disabled on a read failure
        return False


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
    prompt: str, workdir: Path, timeout_s: int, *,
    provider: Optional[str] = None, lane: Optional[Any] = None,
) -> SpawnResult:
    """Run the worker via ``fno agents spawn --substrate headless`` in *workdir*.
    A non-zero exit, missing binary, or timeout is a graded failure, never a
    sweep crash. A *lane* is a complete coordinate: its harness wins over *provider*."""
    name = f"eval-{os.getpid()}-{int(time.time())}"
    cmd = [
        "fno", "agents", "spawn", "--name", name,
        "--substrate", "headless", "--cwd", str(workdir),
        "--timeout", str(timeout_s),
    ]
    harness = (lane.harness if lane else None) or provider
    if harness:
        cmd += ["--harness", harness]
    if lane:
        for flag, val in (("--model", lane.model), ("--effort", lane.effort),
                          ("--route", lane.route), ("--account", lane.account)):
            if val:
                cmd += [flag, val]
    # Behind `--` (fno's own click parser honors it, verified both
    # directions): a leading-flag seed must be the prompt positional.
    cmd += ["--", prompt]
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
        return SpawnResult(False, f"spawn exit {proc.returncode}: {tail[0]}", worker_name=name)
    return SpawnResult(True, worker_name=name)


def _make_disposable_worktree(repo_root: Path, ref: str, tag: str) -> Path:
    from fno.worktree_paths import worktree_base

    base = worktree_base() / "evals"
    base.mkdir(parents=True, exist_ok=True)
    path = base / f"{tag}-{os.getpid()}-{int(time.time()*1000)}"
    subprocess.run(
        ["git", "worktree", "add", "--detach", str(path), ref],
        cwd=str(repo_root), capture_output=True, text=True, check=True,
    )
    # Untracked marker: an in-flight eval tree is a clean detached checkout,
    # which a concurrent `worktree cleanup --merged --apply` reads as a reap
    # candidate between spawn legs; the dirt blocks it via wt_reapable. The
    # age sweep (`cleanup --older-than`) consults wt_reapable on detached
    # trees for the same reason, so the marker holds there too.
    # _remove_worktree's --force and sweep_orphans never look at dirt, so the
    # marker costs nothing on the owned removal paths.
    (path / ".fno-evals-tree").write_text("in-flight eval worktree\n")
    return path


def _remove_worktree(repo_root: Path, path: Path) -> None:
    # Best-effort: a failed removal must never mask the run's verdict. The next
    # `fno doctor evals run` sweeps orphans (see sweep_orphans).
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
    variant: str = "baseline",
    variant_ref: Optional[str] = None,
    lane: Optional[Any] = None,
    experiment_id: Optional[str] = None,
    observe: Optional[Callable[[str], Optional[dict]]] = None,
) -> list[RunResult]:
    """Run *task* ``repeat`` times, appending one history row per run.

    Each run: fresh disposable worktree -> optional worker (skipped for a
    grade-only task) -> mechanical grade -> history row -> worktree removed.
    A worker-spawn failure is a graded fail; the remaining repeats still run.
    A requested *lane* is recorded as the requested coordinate; *observe*
    (default _observe_worker) reads back what actually ran. *experiment_id*
    is an opaque cohort tag recorded on the row.
    """
    if not VARIANT_RE.match(variant):
        raise ValueError(f"variant must match baseline|v<N>, got {variant!r}")
    if variant == BASELINE:
        if variant_ref is not None:
            raise ValueError("variant_ref is not allowed when variant is baseline")
        variant_ref = task.repo_fixture
    elif variant_ref is None:
        raise ValueError(f"variant_ref is required when variant is {variant!r}")
    checkout_ref = variant_ref

    # No injected spawn: bind provider/lane so they actually route the worker.
    spawn_fn = spawn or (
        lambda p, w, t: _default_spawn(p, w, t, provider=worker_provider, lane=lane)
    )
    observe_fn = observe or _observe_worker
    if history_path is None:
        from fno import paths as _paths
        history_path = _paths.evals_history()
    bank_rev = _git_rev(repo_root, checkout_ref)
    timeout_s = max(1, task.timeout_minutes * 60)
    results: list[RunResult] = []

    for i in range(repeat):
        started = time.monotonic()
        reason = ""
        outcome: Optional[GradeOutcome] = None
        workdir: Optional[Path] = None
        worker_name = ""
        spawned = False
        try:
            workdir = _make_disposable_worktree(repo_root, checkout_ref, task.id)
        except subprocess.CalledProcessError as exc:
            # Fixture checkout failed: graded fail with a drift hint, not a crash.
            reason = f"fixture checkout failed ({checkout_ref}); fixture drift? {exc.stderr or ''}".strip()

        if workdir is not None:
            if task.prompt and spawn is None and not evals_enabled():
                # x-aaaf wave 2: the gate only bites the REAL default spawn -
                # an injected spawn_fn (tests, or a caller with its own
                # worker) is an explicit invocation, not autonomous.
                reason = "config.evals.enabled is false"
            elif task.prompt:
                spawn_res = spawn_fn(task.prompt, workdir, timeout_s)
                worker_name = spawn_res.worker_name
                if not spawn_res.ok:
                    reason = spawn_res.reason
                else:
                    spawned = True
            if not reason:
                outcome = grade(task, workdir)
                if not outcome.passed:
                    reason = outcome.reason
            _remove_worktree(repo_root, workdir)

        observed = observe_fn(worker_name) if spawned and worker_name else None
        lane_evidence = _lane_evidence(lane, observed, attempted=bool(task.prompt))

        duration = round(time.monotonic() - started, 3)
        passed = outcome is not None and outcome.passed
        results.append(RunResult(
            task_id=task.id, tier=task.tier, passed=passed,
            reason="" if passed else reason, duration_s=duration, repeat_index=i,
            variant=variant,
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
            "variant": variant,
            "experiment_id": experiment_id,
            **lane_evidence,
        })

    return results
