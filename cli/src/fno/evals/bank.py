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

import subprocess
import warnings
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

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
    assert isinstance(tier, str)

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
        grade=checks,
        repo_fixture=str(raw.get("repo_fixture", "HEAD")),
        timeout_minutes=timeout,
        tags=list(tags_raw),
        source_path=path,
    )


class LaneError(ValueError):
    """A requested lane name has no matching resolved inventory row."""


def resolve_lane(name: str, *, settings: object = None):
    """Resolve a named lane through the existing route_resolve inventory -
    the same fold ``agents.profiles.*.lanes`` joins against, never a second
    model/effort enum. Raises :class:`LaneError` naming known lanes on a miss."""
    from fno.route_resolve import resolve_inventory

    inventory = resolve_inventory(settings=settings)
    row = inventory.rows.get(name)
    if row is None:
        raise LaneError(f"unknown lane {name!r}; declared lanes: {sorted(inventory.rows)}")
    return row


class CohortError(ValueError):
    """A cohort declaration is missing, malformed, stale, or fails native
    validation - qualification and tuning exports refuse on it."""


COHORTS_FILENAME = "cohorts.yaml"


@dataclass(frozen=True)
class CohortDecl:
    """A declared train/validation/qualification split, pinned to a bank rev.

    Stored exactly as declared (lists default empty when a role key is
    absent); the NATIVE door is the semantic authority on membership rules -
    load_cohorts only parses the YAML shape.
    """

    bank_rev: str
    train: list[str] = field(default_factory=list)
    validation: list[str] = field(default_factory=list)
    qualification: list[str] = field(default_factory=list)
    source_path: Optional[Path] = None


def load_cohorts(bank_dir: Path) -> Optional[CohortDecl]:
    """Load ``cohorts.yaml`` from *bank_dir*; None = no declared split.

    Raises :class:`CohortError` on a malformed file (not a mapping, or a
    non-string ``bank_rev`` / non-list role key).
    """
    path = bank_dir / COHORTS_FILENAME
    if not path.exists():
        return None
    try:
        raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise CohortError(f"cannot read {path}: {exc}") from exc
    except yaml.YAMLError as exc:
        raise CohortError(f"malformed YAML in {path}: {exc}") from exc
    if not isinstance(raw, dict):
        raise CohortError(f"{path}: top level must be a mapping")
    bank_rev = raw.get("bank_rev")
    if not isinstance(bank_rev, str) or not bank_rev.strip():
        raise CohortError(f"{path}: 'bank_rev' must be a non-empty string (the pinned revision)")
    roles: dict[str, list[str]] = {}
    for role in ("train", "validation", "qualification"):
        val = raw.get(role, [])
        if not isinstance(val, list) or not all(isinstance(t, str) for t in val):
            raise CohortError(f"{path}: '{role}' must be a list of task ids")
        roles[role] = val
    return CohortDecl(
        bank_rev=bank_rev,
        train=roles["train"],
        validation=roles["validation"],
        qualification=roles["qualification"],
        source_path=path,
    )


def bank_unchanged_since(decl: CohortDecl, repo_root: Path) -> bool:
    """True when every bank task file is unchanged from *decl*'s pinned rev.

    The split pins the BANK, not the whole repo: an unrelated commit since
    the pin keeps the split valid; any change under the bank dir voids it.
    An unreadable pin (unknown rev) voids it too.
    """
    try:
        verify = subprocess.run(
            ["git", "rev-parse", "--verify", "--quiet", f"{decl.bank_rev}^{{commit}}"],
            cwd=str(repo_root), capture_output=True, text=True, timeout=10,
        )
        if verify.returncode != 0:
            return False
        diff = subprocess.run(
            ["git", "diff", "--quiet", f"{decl.bank_rev}", "HEAD", "--", "evals/bank"],
            cwd=str(repo_root), capture_output=True, text=True, timeout=10,
        )
    except Exception:  # noqa: BLE001 - an unreadable git state never certifies the bank
        return False
    return diff.returncode == 0


def _door_binary():
    """The native door binary: this checkout's build outranks any installed copy."""
    from fno.rust_binary import find_dev_binary, resolve_binary

    return find_dev_binary() or resolve_binary()


def cohorts_verdict(
    decl: CohortDecl,
    *,
    known_ids: Optional[list[str]] = None,
    task_ids: Optional[list[str]] = None,
) -> dict[str, Any]:
    """One native membership/eligibility decision shared by run and report
    consumers: `{"ok", "errors", "roles"}`. Fail-closed - an unreachable
    native door is itself a refusal, never a silent pass.

    *known_ids* validates every declared id against the loaded bank;
    *task_ids* resolves each into its cohort role (null when unlisted).
    """
    import json
    import subprocess

    binary = _door_binary()
    if binary is None:
        return {"ok": False, "errors": ["native cohort door unreachable: fno-agents binary not found"],
                "roles": {}}
    payload = json.dumps({
        "bank_rev": decl.bank_rev,
        "train": decl.train,
        "validation": decl.validation,
        "qualification": decl.qualification,
    })
    argv = [str(binary), "evals-attempt", "--cohorts", payload]
    if known_ids is not None:
        argv += ["--known-ids", json.dumps(known_ids)]
    if task_ids is not None:
        argv += ["--task-ids", json.dumps(task_ids)]
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, timeout=30)
    except Exception as exc:  # noqa: BLE001 - a failed door read refuses
        return {"ok": False, "errors": [f"native cohort door failed: {exc}"], "roles": {}}
    if proc.returncode != 0:
        tail = (proc.stderr or proc.stdout or "door failed").strip().splitlines()[-1:]
        return {"ok": False, "errors": [f"native cohort door refused: {tail[0]}"], "roles": {}}
    try:
        verdict = json.loads(proc.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        return {"ok": False, "errors": ["native cohort door returned unreadable output"], "roles": {}}
    if not isinstance(verdict, dict) or "ok" not in verdict:
        return {"ok": False, "errors": ["native cohort door returned an unexpected shape"], "roles": {}}
    return verdict


def discover_bank(bank_dir: Path) -> list[TaskSpec]:
    """Load every ``*.yaml`` under *bank_dir*, sorted by id.

    Raises :class:`BankError` if *bank_dir* is missing (the caller decides
    whether an empty bank is fatal) or any task is invalid. Duplicate ids
    across files are rejected.
    """
    _require(bank_dir.is_dir(), f"bank directory not found: {bank_dir}")
    tasks: dict[str, TaskSpec] = {}
    for yaml_path in sorted(bank_dir.glob("*.yaml")):
        if yaml_path.name == COHORTS_FILENAME:
            continue  # the cohort declaration is not a task
        task = load_task(yaml_path)
        if task.id in tasks:
            raise BankError(
                f"duplicate bank task id '{task.id}': {tasks[task.id].source_path} "
                f"and {yaml_path}"
            )
        tasks[task.id] = task
    return [tasks[k] for k in sorted(tasks)]
