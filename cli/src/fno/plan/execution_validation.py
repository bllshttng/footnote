"""Semantic validation for executable single-doc plans."""
from __future__ import annotations

from collections import Counter, defaultdict
from dataclasses import dataclass
import posixpath
import re
import shlex

from fno.plan._doc import PlanDoc
from fno.plan.brief import BriefParseError, parse_execution_strategy
from fno.plan.criteria import CriteriaParseError, compile_criteria


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
# Command names that are also ordinary English verbs, so a prose line can lead
# with one, plus the determiners that follow them in a sentence but never in an
# invocation. Matching the PAIR is what keeps `cargo check` and `make test`
# runnable while `make sure the button works` is not.
_AMBIGUOUS_COMMANDS = frozenset(
    "cd check ensure go java just make manually node test verify".split()
)
# No `all`: `make all` is a real default target, and it outranks `make all the
# tests pass` as a thing an author writes.
_PROSE_FOLLOWERS = frozenset(
    "a an each every inspect it its my that the tests this to into your sure".split()
)
_PROSE_ONLY_COMMANDS = frozenset("handwave manually".split())
_COMMAND_WRAPPERS = frozenset({"command", "env", "gtimeout", "sudo", "timeout"})
_CONVENTIONAL_FILENAMES = frozenset(
    """
    brewfile changelog codeowners dockerfile gemfile justfile license makefile
    notice procfile rakefile readme vagrantfile
    """.split()
)
# Brackets stay legal: `app/[slug]/page.tsx` is a routing convention, not a glob.
_FILE_REJECT_RE = re.compile(r"""[\s`$&|;<>(){}*?"'!]""")
_FILE_EXTENSION_RE = re.compile(r"[\w.+-]+\.[A-Za-z0-9]+")
_DURATION_RE = re.compile(r"^\d+(?:\.\d+)?[smhd]?$")
_NON_EXECUTABLE_SUFFIXES = (
    ".md", ".txt", ".rst", ".json", ".yaml", ".yml", ".toml", ".lock", ".csv",
)
_ENV_ASSIGNMENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
_SHELL_PARAMETER_RE = re.compile(
    r"\$(?:\{[A-Za-z_][A-Za-z0-9_]*\}|[A-Za-z_][A-Za-z0-9_]*)"
)
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


def _runs_a_command(tokens: list[str]) -> bool:
    while tokens and (tokens[0] == "!" or _ENV_ASSIGNMENT_RE.match(tokens[0])):
        tokens.pop(0)
    while tokens and tokens[0] in _COMMAND_WRAPPERS:
        wrapper = tokens.pop(0)
        while tokens and (tokens[0].startswith("-") or _ENV_ASSIGNMENT_RE.match(tokens[0])):
            tokens.pop(0)
        # `timeout`/`gtimeout` take a positional duration before the command,
        # and `-k` takes one of its own, so drop the whole leading run.
        while tokens and wrapper.endswith("timeout") and _DURATION_RE.match(tokens[0]):
            tokens.pop(0)
    if not tokens:
        return False
    command = tokens[0]
    command_lower = command.lower()
    if command[:1].isupper():
        return False
    if (
        command_lower in _AMBIGUOUS_COMMANDS
        and len(tokens) > 1
        and tokens[1].lower() in _PROSE_FOLLOWERS
    ):
        return False
    if command_lower in _PROSE_ONLY_COMMANDS or command.startswith("-"):
        return False
    command_for_reject = command
    if "$" in command:
        if "/" not in command:
            return False
        command_for_reject = _SHELL_PARAMETER_RE.sub("", command)
        if "$" in command_for_reject:
            return False
    if _FILE_REJECT_RE.search(command_for_reject):
        return False
    # A document-shaped first token is prose that happens to lead with a path;
    # other bare tokens remain valid without an ecosystem-specific inventory.
    return not command_lower.endswith(_NON_EXECUTABLE_SUFFIXES)


def _is_runnable_verify(value: str) -> bool:
    if _is_placeholder_verify(value):
        return False
    try:
        # Whole-value parse first: a well-formed segment must not rescue a value
        # bash itself would reject, such as `pytest tests/ && echo "unclosed`.
        whole = shlex.split(value, comments=False, posix=True)
    except ValueError:
        return False
    for segment in _SHELL_SEGMENT_RE.split(value):
        try:
            tokens = shlex.split(segment, comments=False, posix=True)
        except ValueError:
            continue
        if _runs_a_command(tokens):
            return True
    # Segment splitting is quoting-blind, so `rg -n 'a|b' src/` shreds into two
    # unparseable halves. The whole value already parsed; judge that instead.
    return _runs_a_command(whole)


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


def _is_concrete_file(value: str) -> bool:
    """Quick-plan-only lexical path predicate.

    Never touches the filesystem: a quick plan may name a file it is about to
    create, so validity must not depend on checkout state. Full-plan ``surface``
    values stay symbolic and are deliberately not routed through this.
    """
    token = value.strip().strip("`").strip()
    if not token or _has_quick_placeholder(token):
        return False
    # Matched whole, never as a substring: `src/en/api.ts` contains "n/a".
    if token.lower() in {"n/a", "na", "none", "nil"}:
        return False
    if _FILE_REJECT_RE.search(token) or "://" in token or token.startswith("-"):
        return False
    # Directory syntax names no file. `cli/src` stays accepted because an
    # extensionless path is a legal file (`bin/mycli`) and nothing lexical
    # separates the two; only unambiguous directory shapes are refused.
    if token.endswith("/") or token.rsplit("/", 1)[-1] in {"", ".", ".."}:
        return False
    if "/" in token:
        return True
    return (
        token.lower() in _CONVENTIONAL_FILENAMES
        or (token.startswith(".") and len(token) > 1)
        or bool(_FILE_EXTENSION_RE.fullmatch(token))
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
    if files_body and not any(_is_concrete_file(token) for token in file_tokens):
        violations.append(
            _violation("Files to Modify", "must name at least one concrete file path")
        )

    verification = (doc.get_section("Verification") or "").strip()
    commands = _quick_verification_candidates(verification)
    if verification and not any(_is_runnable_verify(command) for command in commands):
        violations.append(
            _violation("Verification", "must contain at least one concrete runnable command")
        )
    return ExecutionValidationResult(violations)


def _validate_compiled_acceptance(
    doc: PlanDoc, tasks: list[dict]
) -> list[ExecutionViolation]:
    """compiled-v1 reference resolution: every task acceptance ref resolves.

    Compiles the Acceptance Criteria section through the canonical compiler and
    refuses on a malformed/duplicate section or on any task reference that maps
    to no compiled criterion. The diagnostic names the task and the unresolved
    code so an author can fix the reference without decoding the whole plan.
    """
    out: list[ExecutionViolation] = []
    ac_section = doc.get_section("Acceptance Criteria") or ""
    try:
        criteria = compile_criteria(ac_section)
    except CriteriaParseError as exc:
        out.append(_violation("Acceptance Criteria", str(exc)))
        return out
    codes = {c.code for c in criteria}
    for task in tasks:
        task_id = str(task.get("id", "")).strip() or "?"
        refs = task.get("acceptance", [])
        if not isinstance(refs, list):
            continue
        for ref in refs:
            ref_str = str(ref).strip()
            if ref_str and ref_str not in codes:
                out.append(
                    _violation(
                        f"tasks.{task_id}.acceptance",
                        f"references {ref_str!r}, which resolves to no compiled criterion",
                    )
                )
    return out


def validate_execution(
    doc: PlanDoc,
    *,
    acceptance_contract: str | None = None,
) -> ExecutionValidationResult:
    """Validate the representation consumed by workers and wave scheduling.

    ``acceptance_contract`` overrides the frontmatter value so a caller can
    validate a PROPOSED state without writing it first (``mutate_doc.finalize``
    passes ``compiled-v1`` to validate a design->ready promotion before it
    lands). When ``None`` the contract is read from ``doc.frontmatter``. Only a
    ``compiled-v1`` plan gets strict acceptance-reference resolution; an
    unstamped historical plan keeps the permissive legacy read (AC3-COMPAT).
    """
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

    # compiled-v1: every task acceptance reference must resolve to exactly one
    # compiled criterion (AC5-ERR). Unstamped plans skip this (AC3-COMPAT).
    contract = acceptance_contract
    if contract is None:
        raw_contract = doc.frontmatter.get("acceptance_contract")
        contract = str(raw_contract).strip() if raw_contract else None
    if contract == "compiled-v1":
        violations.extend(_validate_compiled_acceptance(doc, tasks))

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
