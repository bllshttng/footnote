"""Mechanical grading: run a task's checks against a graded worktree.

Deterministic by construction (Locked Decision 2): no model in the pass/fail
path. Three check kinds (see :mod:`fno.evals.bank`):
- ``exit``:        a shell command's exit code must equal ``expect``.
- ``file-exists``: a workdir-relative path must exist.
- ``grep``:        a workdir-relative file must contain a substring.

The same worktree end-state always grades the same (Invariant).
"""
from __future__ import annotations

import subprocess
from dataclasses import dataclass, field
from pathlib import Path

from fno.evals.bank import GradeCheck, TaskSpec


@dataclass(frozen=True)
class CheckResult:
    kind: str
    passed: bool
    detail: str


@dataclass
class GradeOutcome:
    passed: bool
    results: list[CheckResult] = field(default_factory=list)

    @property
    def reason(self) -> str:
        """One-line reason for a failure (first failing check), or ''."""
        for r in self.results:
            if not r.passed:
                return r.detail
        return ""


def _grade_exit(check: GradeCheck, workdir: Path, timeout_s: int) -> CheckResult:
    command = check.command or ""
    try:
        proc = subprocess.run(
            command, shell=True, cwd=str(workdir),
            capture_output=True, text=True, timeout=timeout_s,
        )
    except subprocess.TimeoutExpired:
        return CheckResult("exit", False, f"exit: `{command}` timed out after {timeout_s}s")
    ok = proc.returncode == check.expect
    detail = f"exit: `{command}` -> {proc.returncode} (want {check.expect})"
    return CheckResult("exit", ok, detail)


def _grade_file_exists(check: GradeCheck, workdir: Path) -> CheckResult:
    target = workdir / (check.path or "")
    ok = target.exists()
    return CheckResult("file-exists", ok, f"file-exists: {check.path} -> {'present' if ok else 'missing'}")


def _grade_grep(check: GradeCheck, workdir: Path) -> CheckResult:
    target = workdir / (check.path or "")
    pattern = check.pattern or ""
    if not target.is_file():
        return CheckResult("grep", False, f"grep: {check.path} not a file")
    try:
        text = target.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        return CheckResult("grep", False, f"grep: cannot read {check.path}: {exc}")
    ok = pattern in text
    return CheckResult("grep", ok, f"grep: {pattern!r} in {check.path} -> {'found' if ok else 'absent'}")


def grade(task: TaskSpec, workdir: Path) -> GradeOutcome:
    """Run every check in *task* against *workdir*; passed iff all pass.

    The per-check timeout for ``exit`` checks derives from the task's
    ``timeout_minutes`` (the same budget that bounds the worker step).
    """
    timeout_s = max(1, task.timeout_minutes * 60)
    results: list[CheckResult] = []
    for check in task.grade:
        if check.kind == "exit":
            results.append(_grade_exit(check, workdir, timeout_s))
        elif check.kind == "file-exists":
            results.append(_grade_file_exists(check, workdir))
        else:  # grep (validated at load time)
            results.append(_grade_grep(check, workdir))
    return GradeOutcome(passed=all(r.passed for r in results), results=results)
