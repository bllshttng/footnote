"""Semantic validation for executable single-doc plans."""
from __future__ import annotations

from collections import Counter, defaultdict
from dataclasses import dataclass

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
_VALID_MODES = {"sequential", "parallel", "mixed"}


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


def _normalize_refs(value: object) -> list[str] | None:
    if not isinstance(value, list):
        return None
    return [str(item).strip() for item in value]


def _validate_quick(doc: PlanDoc) -> ExecutionValidationResult:
    violations = []
    for section in _QUICK_REQUIRED_SECTIONS:
        if not (doc.get_section(section) or "").strip():
            violations.append(_violation(section, f"missing or empty ## {section} section"))
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
    if mode not in _VALID_MODES:
        violations.append(
            _violation(
                "execution_mode",
                f"expected one of {sorted(_VALID_MODES)}, got {mode!r}",
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
        if _is_placeholder_verify(verify):
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

    wave_ids = [str(wave.get("wave", "")).strip() for wave in waves if isinstance(wave, dict)]
    duplicate_wave_ids = sorted(wave_id for wave_id, count in Counter(wave_ids).items() if wave_id and count > 1)
    for wave_id in duplicate_wave_ids:
        violations.append(_violation(f"waves.{wave_id}.wave", f"duplicate wave id '{wave_id}'"))

    declared_wave_ids = {wave_id for wave_id in wave_ids if wave_id}
    referenced_tasks: list[str] = []
    dependency_graph: dict[str, set[str]] = {wave_id: set() for wave_id in declared_wave_ids}
    for index, wave in enumerate(waves, start=1):
        if not isinstance(wave, dict):
            violations.append(_violation(f"waves.{index}", "wave must be a mapping"))
            continue
        wave_id = str(wave.get("wave", "")).strip()
        label = wave_id or str(index)
        if not wave_id:
            violations.append(_violation(f"waves.{label}.wave", "wave id is required"))

        wave_mode = str(wave.get("mode", "")).strip()
        if wave_mode not in _VALID_MODES:
            violations.append(
                _violation(
                    f"waves.{label}.mode",
                    f"expected one of {sorted(_VALID_MODES)}, got {wave_mode!r}",
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
                elif wave_id:
                    dependency_graph[wave_id].add(dependency_id)

        if wave_mode == "parallel":
            owners: dict[str, list[str]] = defaultdict(list)
            for ref in refs:
                task = tasks_by_id.get(ref)
                if not task:
                    continue
                for path in task.get("surface", []):
                    normalized = str(path).strip()
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
