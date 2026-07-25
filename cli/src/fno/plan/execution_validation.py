"""Semantic validation for executable single-doc plans."""
from __future__ import annotations

from collections import Counter, defaultdict
from dataclasses import dataclass
import posixpath
import re
import shlex

from fno.plan._doc import PlanDoc
from fno.plan.brief import BriefParseError, parse_execution_strategy


@dataclass(frozen=True)
class ExecutionViolation:
    field: str
    message: str


@dataclass(frozen=True)
class ExecutionValidationResult:
    violations: list[ExecutionViolation]


_QUICK_REQUIRED_SECTIONS = ("Context", "Changes", "Files to Modify", "Verification")
_EXECUTION_MODES = {"sequential", "parallel", "mixed"}
_WAVE_MODES = {"sequential", "parallel"}
_TASK_ID_RE = re.compile(r"^\d+(?:\.\d+)*[A-Za-z]*$")
_QUICK_PLACEHOLDERS = (
    "[change name]",
    "[describe the change",
    "[concrete runnable check]",
    "[how to verify",
    "path/to/",
    "replace me",
    "fill in",
)
_RUNNABLE_COMMANDS = frozenset(
    """
    [ bash bun bundle cargo cd composer curl deno docker dotnet fno fno-agents
    git go gradle grep gh helm java javac jq just make mvn mypy node npm npx php
    pnpm podman pre-commit pytest python python3 rg rspec ruby ruff rustc sh
    shellcheck swift terraform test tofu uv wget xcodebuild yarn yq zsh
    """.split()
)
_COMMAND_WRAPPERS = frozenset({"command", "env", "gtimeout", "sudo", "timeout"})
_ENV_ASSIGNMENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
_SHELL_SEGMENT_RE = re.compile(r"(?:&&|\|\||[;|])")


def _violation(field: str, message: str) -> ExecutionViolation:
    return ExecutionViolation(field=field, message=message)


def _is_placeholder_verify(value: str) -> bool:
    normalized = value.strip().lower()
    return (
        not normalized
        or normalized.startswith("#")
        or "fill in" in normalized
        or normalized in {"none", "null", "todo", "tbd", "replace me"}
    )


def _is_runnable_verify(value: str) -> bool:
    if _is_placeholder_verify(value):
        return False
    for segment in _SHELL_SEGMENT_RE.split(value):
        try:
            tokens = shlex.split(segment, comments=False, posix=True)
        except ValueError:
            continue
        while tokens and (tokens[0] == "!" or _ENV_ASSIGNMENT_RE.match(tokens[0])):
            tokens.pop(0)
        while tokens and tokens[0] in _COMMAND_WRAPPERS:
            tokens.pop(0)
            while tokens and (tokens[0].startswith("-") or _ENV_ASSIGNMENT_RE.match(tokens[0])):
                tokens.pop(0)
        if not tokens:
            continue
        command = tokens[0]
        if command in _RUNNABLE_COMMANDS or "/" in command:
            return True
    return False


def _normalize_refs(value: object) -> list[str] | None:
    if not isinstance(value, list):
        return None
    return [str(item).strip() for item in value]


def _has_quick_placeholder(value: str) -> bool:
    lowered = value.strip().lower()
    return (
        any(marker in lowered for marker in _QUICK_PLACEHOLDERS)
        or lowered.strip("[] .:") in {"todo", "tbd"}
    )


def _quick_file_candidates(body: str) -> list[str]:
    candidates = re.findall(r"`([^`\n]+)`", body)
    for raw_line in body.splitlines():
        line = raw_line.strip()
        if not line or re.fullmatch(r"\|?[\s:|-]+\|?", line):
            continue
        if line.startswith("|"):
            cells = [cell.strip().strip("`") for cell in line.strip("|").split("|")]
            if cells and cells[0].lower() != "file":
                candidates.append(cells[0])
            continue
        bullet = re.match(r"^[-*+]\s+([^\s]+)", line)
        if bullet:
            candidates.append(bullet.group(1).strip("`"))
    return candidates


def _quick_verification_candidates(body: str) -> list[str]:
    candidates = re.findall(r"`([^`\n]+)`", body)
    for raw_line in body.splitlines():
        line = re.sub(r"^\s*(?:[-*+] |\d+[.)] )", "", raw_line).strip()
        if line and not line.startswith(("```", "|")):
            candidates.append(line)
    return candidates


def _validate_quick(doc: PlanDoc) -> ExecutionValidationResult:
    violations = []
    for section in _QUICK_REQUIRED_SECTIONS:
        if not (doc.get_section(section) or "").strip():
            violations.append(_violation(section, f"missing or empty ## {section} section"))
    for section in ("Changes", "Files to Modify", "Verification"):
        body = (doc.get_section(section) or "").strip()
        lowered = body.lower()
        if body and _has_quick_placeholder(lowered):
            violations.append(
                _violation(section, f"## {section} contains template placeholder content")
            )

    files_body = (doc.get_section("Files to Modify") or "").strip()
    file_tokens = _quick_file_candidates(files_body)
    if files_body and not any(
        token.strip() and not _has_quick_placeholder(token)
        for token in file_tokens
    ):
        violations.append(
            _violation("Files to Modify", "must name at least one concrete file path")
        )

    verification = (doc.get_section("Verification") or "").strip()
    commands = _quick_verification_candidates(verification)
    if verification and not any(
        command.strip()
        and not _has_quick_placeholder(command)
        for command in commands
    ):
        violations.append(
            _violation("Verification", "must contain at least one concrete runnable command")
        )
    return ExecutionValidationResult(violations)


def validate_execution(doc: PlanDoc) -> ExecutionValidationResult:
    """Validate the representation consumed by workers and wave scheduling."""
    strategy_text = doc.get_section("Execution Strategy")
    if strategy_text is None:
        if doc.frontmatter.get("kind") == "quick-plan":
            return _validate_quick(doc)
        return ExecutionValidationResult(
            [_violation("Execution Strategy", "missing ## Execution Strategy section")]
        )

    try:
        strategy = parse_execution_strategy(strategy_text)
    except (BriefParseError, TypeError, ValueError) as exc:
        return ExecutionValidationResult(
            [_violation("Execution Strategy", str(exc))]
        )

    violations: list[ExecutionViolation] = []
    mode = str(strategy.get("execution_mode", "")).strip()
    if mode not in _EXECUTION_MODES:
        violations.append(
            _violation(
                "execution_mode",
                f"expected one of {sorted(_EXECUTION_MODES)}, got {mode!r}",
            )
        )

    tasks = strategy.get("tasks", [])
    if not tasks:
        violations.append(_violation("tasks", "Execution Strategy must declare at least one task"))

    task_ids = [str(task.get("id", "")).strip() for task in tasks]
    duplicate_task_ids = sorted(task_id for task_id, count in Counter(task_ids).items() if task_id and count > 1)
    for task_id in duplicate_task_ids:
        violations.append(_violation(f"tasks.{task_id}.id", f"duplicate task id '{task_id}'"))

    tasks_by_id: dict[str, dict] = {}
    for index, task in enumerate(tasks, start=1):
        task_id = str(task.get("id", "")).strip()
        label = task_id or str(index)
        if not task_id:
            violations.append(_violation(f"tasks.{label}.id", "task id is required"))
        elif not _TASK_ID_RE.fullmatch(task_id):
            violations.append(
                _violation(
                    f"tasks.{label}.id",
                    "task id must match the durable state grammar (for example 1.1 or 02b)",
                )
            )
        else:
            tasks_by_id.setdefault(task_id, task)

        if not str(task.get("title", "")).strip():
            violations.append(_violation(f"tasks.{label}.title", "task title is required"))

        surface = task.get("surface", [])
        if not isinstance(surface, list) or not [item for item in surface if str(item).strip()]:
            violations.append(
                _violation(f"tasks.{label}.surface", "task surface must name at least one file")
            )

        verify = str(task.get("verify", ""))
        if not _is_runnable_verify(verify):
            violations.append(
                _violation(f"tasks.{label}.verify", "task verify must be a concrete runnable command")
            )

        acceptance = task.get("acceptance", [])
        if not isinstance(acceptance, list) or not [item for item in acceptance if str(item).strip()]:
            violations.append(
                _violation(
                    f"tasks.{label}.acceptance",
                    "task acceptance must contain at least one criterion",
                )
            )

    waves = strategy.get("waves", [])
    if not isinstance(waves, list) or not waves:
        violations.append(_violation("waves", "Execution Strategy must declare at least one wave"))
        return ExecutionValidationResult(violations)

    wave_ids = [
        str(wave.get("wave"))
        for wave in waves
        if isinstance(wave, dict)
        and isinstance(wave.get("wave"), int)
        and not isinstance(wave.get("wave"), bool)
        and wave["wave"] > 0
    ]
    duplicate_wave_ids = sorted(wave_id for wave_id, count in Counter(wave_ids).items() if wave_id and count > 1)
    for wave_id in duplicate_wave_ids:
        violations.append(_violation(f"waves.{wave_id}.wave", f"duplicate wave id '{wave_id}'"))

    declared_wave_ids = {wave_id for wave_id in wave_ids if wave_id}
    wave_positions = {wave_id: index for index, wave_id in enumerate(wave_ids)}
    referenced_tasks: list[str] = []
    dependency_graph: dict[str, set[str]] = {wave_id: set() for wave_id in declared_wave_ids}
    for index, wave in enumerate(waves, start=1):
        if not isinstance(wave, dict):
            violations.append(_violation(f"waves.{index}", "wave must be a mapping"))
            continue
        wave_value = wave.get("wave")
        wave_id = str(wave_value).strip() if wave_value is not None else ""
        label = wave_id or str(index)
        if (
            not isinstance(wave_value, int)
            or isinstance(wave_value, bool)
            or wave_value <= 0
        ):
            violations.append(
                _violation(f"waves.{label}.wave", "wave id must be a positive integer")
            )

        wave_mode = str(wave.get("mode", "")).strip()
        if wave_mode not in _WAVE_MODES:
            violations.append(
                _violation(
                    f"waves.{label}.mode",
                    f"expected one of {sorted(_WAVE_MODES)}, got {wave_mode!r}",
                )
            )

        refs = _normalize_refs(wave.get("tasks"))
        if refs is None or not [ref for ref in refs if ref]:
            violations.append(
                _violation(f"waves.{label}.tasks", "wave must reference at least one task")
            )
            refs = []
        referenced_tasks.extend(refs)
        for ref in refs:
            if ref not in tasks_by_id:
                violations.append(
                    _violation(f"waves.{label}.tasks", f"wave references unknown task '{ref}'")
                )

        dependencies = wave.get("depends_on", [])
        if dependencies not in (None, ""):
            if not isinstance(dependencies, list):
                dependencies = [dependencies]
            for dependency in dependencies:
                dependency_id = str(dependency).strip()
                if dependency_id not in declared_wave_ids:
                    violations.append(
                        _violation(
                            f"waves.{label}.depends_on",
                            f"wave references unknown dependency '{dependency_id}'",
                        )
                    )
                elif wave_id in wave_positions:
                    dependency_graph[wave_id].add(dependency_id)
                    if wave_positions[dependency_id] >= wave_positions[wave_id]:
                        violations.append(
                            _violation(
                                f"waves.{label}.depends_on",
                                f"dependency '{dependency_id}' must reference an earlier declared wave",
                            )
                        )

        if wave_mode == "parallel":
            owners: dict[str, list[str]] = defaultdict(list)
            for ref in refs:
                task = tasks_by_id.get(ref)
                if not task:
                    continue
                for path in task.get("surface", []):
                    normalized = posixpath.normpath(str(path).strip())
                    if normalized:
                        owners[normalized].append(ref)
            for path, owner_ids in sorted(owners.items()):
                if len(owner_ids) > 1:
                    violations.append(
                        _violation(
                            f"waves.{label}.surface",
                            f"parallel tasks share surface '{path}': {', '.join(owner_ids)}",
                        )
                    )

    for task_id, count in sorted(Counter(referenced_tasks).items()):
        if task_id in tasks_by_id and count > 1:
            violations.append(
                _violation(f"tasks.{task_id}", f"task is referenced {count} times")
            )
    for task_id in sorted(set(tasks_by_id) - set(referenced_tasks)):
        violations.append(_violation(f"tasks.{task_id}", "task is not referenced by any wave"))

    visiting: set[str] = set()
    visited: set[str] = set()

    def _has_cycle(wave_id: str) -> bool:
        if wave_id in visiting:
            return True
        if wave_id in visited:
            return False
        visiting.add(wave_id)
        for dependency_id in dependency_graph.get(wave_id, set()):
            if _has_cycle(dependency_id):
                return True
        visiting.remove(wave_id)
        visited.add(wave_id)
        return False

    if any(_has_cycle(wave_id) for wave_id in sorted(dependency_graph)):
        violations.append(_violation("waves.depends_on", "wave dependency cycle detected"))

    return ExecutionValidationResult(violations)
