"""Golden-task eval bank: task format + loader + load-time validation.

A bank task is one ``evals/bank/<id>.yaml`` file:

    id: merged-clean-rate
    tier: regression          # capability | regression
    prompt: |
      <the task the worker is asked to perform>
    repo_fixture: HEAD        # git ref or fixture dir (optional; default HEAD)
    grade:                    # >=1 mechanical check; a gradeless task is invalid
      - kind: exit            # command exit code must equal `expect` (default 0)
        command: "pytest -q"
      - kind: file-exists
        path: "out/report.md"
      - kind: grep
        path: "out/report.md"
        pattern: "PASS"
    timeout_minutes: 15
    tags: [ci-flake]

The two disciplines this enforces at load time (develop-tests.md):
1. Success criteria are mechanical - a task without a runnable ``grade`` is
   rejected naming the id and file (AC4-EDGE).
2. Grades must be specific - an all-``true`` (decorative) grade warns so a task
   cannot silently always-pass (the silent-failure-hunter countermeasure).
"""
from __future__ import annotations

import warnings
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

import yaml


VALID_TIERS = ("capability", "regression")
VALID_CHECK_KINDS = ("exit", "file-exists", "grep")

# Commands whose exit code says nothing task-specific: an exit-only grade built
# solely from these is decorative (always green). ponytail: a fixed no-op set is
# enough to catch the `true` case the plan names; a token-level "does the command
# mention the task" analysis is the upgrade path if decorative grades slip past.
_TRIVIAL_COMMANDS = frozenset({"true", ":", ""})


class BankError(ValueError):
    """A bank task is malformed or violates the load-time discipline.

    The message always names the offending task id (when known) and file so a
    load failure points straight at the YAML to fix.
    """


@dataclass(frozen=True)
class GradeCheck:
    """One mechanical grade check. ``kind`` selects the params that matter."""

    kind: str
    command: Optional[str] = None   # kind=exit
    expect: int = 0                 # kind=exit: required exit code
    path: Optional[str] = None      # kind=file-exists | grep (workdir-relative)
    pattern: Optional[str] = None   # kind=grep


@dataclass(frozen=True)
class TaskSpec:
    """A loaded, validated bank task.

    ``prompt`` is optional: a task with no prompt is *grade-only* - the runner
    skips the worker spawn and grades the fixture directly. This is the honest
    model for a CI-flake regression task (run an existing suite K times; there
    is no agent work to do, only a flake to measure).
    """

    id: str
    tier: str
    grade: list[GradeCheck]
    prompt: Optional[str] = None
    repo_fixture: str = "HEAD"
    timeout_minutes: int = 15
    tags: list[str] = field(default_factory=list)
    source_path: Optional[Path] = None


def _require(cond: bool, msg: str) -> None:
    if not cond:
        raise BankError(msg)


def _parse_check(raw: object, *, task_id: str, path: Path, index: int) -> GradeCheck:
    where = f"task '{task_id}' ({path}) grade[{index}]"
    _require(isinstance(raw, dict), f"{where}: each grade check must be a mapping")
    assert isinstance(raw, dict)  # for type-checkers; _require raised otherwise
    kind = raw.get("kind")
    _require(
        kind in VALID_CHECK_KINDS,
        f"{where}: kind must be one of {VALID_CHECK_KINDS}, got {kind!r}",
    )
    if kind == "exit":
        command = raw.get("command")
        _require(isinstance(command, str) and command.strip() != "",
                 f"{where}: kind=exit requires a non-empty 'command'")
        expect = raw.get("expect", 0)
        _require(isinstance(expect, int), f"{where}: 'expect' must be an int")
        return GradeCheck(kind="exit", command=command, expect=expect)
    if kind == "file-exists":
        fpath = raw.get("path")
        _require(isinstance(fpath, str) and fpath.strip() != "",
                 f"{where}: kind=file-exists requires a non-empty 'path'")
        return GradeCheck(kind="file-exists", path=fpath)
    # grep
    fpath = raw.get("path")
    pattern = raw.get("pattern")
    _require(isinstance(fpath, str) and fpath.strip() != "",
             f"{where}: kind=grep requires a non-empty 'path'")
    _require(isinstance(pattern, str) and pattern != "",
             f"{where}: kind=grep requires a non-empty 'pattern'")
    return GradeCheck(kind="grep", path=fpath, pattern=pattern)


def _warn_if_decorative(task_id: str, path: Path, checks: list[GradeCheck]) -> None:
    """Warn when every check is exit-only against a trivial no-op command.

    A grade like ``[{kind: exit, command: true}]`` always passes, making the
    task decorative. This is a warning, not an error: a legitimate task may
    exit-check a real command, and we cannot prove intent mechanically.
    """
    all_trivial_exit = checks and all(
        c.kind == "exit" and (c.command or "").strip() in _TRIVIAL_COMMANDS
        for c in checks
    )
    if all_trivial_exit:
        warnings.warn(
            f"bank task '{task_id}' ({path}): every grade check is a trivial "
            f"exit-only command ({_TRIVIAL_COMMANDS}); this task always passes "
            f"and grades nothing task-specific.",
            stacklevel=2,
        )


def load_task(path: Path) -> TaskSpec:
    """Load and validate one bank YAML file.

    Raises :class:`BankError` (naming the id and file) on any structural or
    discipline violation - most importantly a missing or empty ``grade``.
    """
    try:
        raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise BankError(f"cannot read bank task {path}: {exc}") from exc
    except yaml.YAMLError as exc:
        raise BankError(f"malformed YAML in bank task {path}: {exc}") from exc

    _require(isinstance(raw, dict), f"bank task {path}: top level must be a mapping")
    assert isinstance(raw, dict)

    task_id = raw.get("id")
    _require(isinstance(task_id, str) and task_id.strip() != "",
             f"bank task {path}: missing 'id'")
    assert isinstance(task_id, str)

    tier = raw.get("tier")
    _require(tier in VALID_TIERS,
             f"task '{task_id}' ({path}): tier must be one of {VALID_TIERS}, got {tier!r}")

    prompt = raw.get("prompt")
    _require(prompt is None or isinstance(prompt, str),
             f"task '{task_id}' ({path}): 'prompt' must be a string when present")
    if isinstance(prompt, str) and prompt.strip() == "":
        prompt = None  # blank prompt == grade-only

    grade_raw = raw.get("grade")
    _require(isinstance(grade_raw, list) and len(grade_raw) > 0,
             f"task '{task_id}' ({path}): 'grade' must be a non-empty list of "
             f"mechanical checks (a task with no runnable grade is invalid)")
    assert isinstance(grade_raw, list)
    checks = [
        _parse_check(c, task_id=task_id, path=path, index=i)
        for i, c in enumerate(grade_raw)
    ]
    _warn_if_decorative(task_id, path, checks)

    timeout = raw.get("timeout_minutes", 15)
    _require(isinstance(timeout, int) and timeout > 0,
             f"task '{task_id}' ({path}): 'timeout_minutes' must be a positive int")

    tags_raw = raw.get("tags") or []
    _require(isinstance(tags_raw, list) and all(isinstance(t, str) for t in tags_raw),
             f"task '{task_id}' ({path}): 'tags' must be a list of strings")

    return TaskSpec(
        id=task_id,
        tier=tier,
        prompt=prompt,
        grade=checks,  # type: ignore[arg-type]
        repo_fixture=str(raw.get("repo_fixture", "HEAD")),
        timeout_minutes=timeout,
        tags=list(tags_raw),
        source_path=path,
    )


def discover_bank(bank_dir: Path) -> list[TaskSpec]:
    """Load every ``*.yaml`` under *bank_dir*, sorted by id.

    Raises :class:`BankError` if *bank_dir* is missing (the caller decides
    whether an empty bank is fatal) or any task is invalid. Duplicate ids
    across files are rejected.
    """
    _require(bank_dir.is_dir(), f"bank directory not found: {bank_dir}")
    tasks: dict[str, TaskSpec] = {}
    for yaml_path in sorted(bank_dir.glob("*.yaml")):
        task = load_task(yaml_path)
        if task.id in tasks:
            raise BankError(
                f"duplicate bank task id '{task.id}': {tasks[task.id].source_path} "
                f"and {yaml_path}"
            )
        tasks[task.id] = task
    return [tasks[k] for k in sorted(tasks)]
