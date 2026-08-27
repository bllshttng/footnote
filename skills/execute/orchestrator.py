#!/usr/bin/env python3
"""
Wave orchestration logic for /fno:execute waves

Handles:
- Parsing execution strategy from the plan doc
- Determining wave execution order
- Tracking wave completion status
- Resume from STATE.md
- Agent routing based on task tags/keywords
"""

import sys
import re
import os
import json
import subprocess
from enum import Enum
from pathlib import Path
from dataclasses import dataclass, field
from typing import Collection, List, Literal, Optional, Dict, Set
from datetime import datetime

# ---------------------------------------------------------------------------
# Single-doc plan support imports (fno.plan._doc)
# ---------------------------------------------------------------------------

# Lazily resolve the CLI src directory relative to this file's canonical
# location so the orchestrator works whether invoked from the skills/ tree or
# from a test suite that adds the skills/ dir to sys.path.
_SKILLS_DIR = Path(__file__).resolve().parent
_CLI_SRC = _SKILLS_DIR.parents[1] / "cli" / "src"
if str(_CLI_SRC) not in sys.path:
    sys.path.insert(0, str(_CLI_SRC))


# ---------------------------------------------------------------------------
# Impeccable executor constants
# ---------------------------------------------------------------------------

IMPECCABLE_DEFAULT_MAX_ITERATIONS: int = 8
IMPECCABLE_DEFAULT_CRITIQUE_TARGET: int = 35
IMPECCABLE_DEFAULT_CRITIQUE_FLOOR: int = 25

# PRODUCT.md validation thresholds (mirrors /impeccable's loader contract)
PRODUCT_MD_MIN_CHARS: int = 200
PRODUCT_MD_TODO_DOMINANCE_RATIO: float = 0.25


# ---------------------------------------------------------------------------
# PRODUCT.md dispatch-time gate (decision 3a, dispatch half)
# ---------------------------------------------------------------------------

# Search order for PRODUCT.md - mirrors /blueprint's and /impeccable's loader contract.
_PRODUCT_MD_SEARCH_PATHS = (
    "PRODUCT.md",
    ".agents/context/PRODUCT.md",
    "docs/PRODUCT.md",
)


# ---------------------------------------------------------------------------
# Company topology projection (x-7741)
# ---------------------------------------------------------------------------
# The router projects a resolved company topology onto a work order through
# fno.company.execution.project_execution, which carries the shape alongside the
# work order's unchanged identity, authority ceiling, approval floor, and
# required evidence. There is no second router: topology changes execution
# shape only, so the same WorkOrderRef and delivery requirements pass through
# direct, loop, squad, and pipeline unchanged. Company work is not yet flowing
# through this router (x-edf5 is the first consumer); this is the seam.


def apply_company_topology(resolution, work_order, authority_ceiling, approval_floor, required_evidence_ids):
    """Project a resolved topology for a company work order without altering truth."""
    from fno.company.execution import project_execution

    return project_execution(
        resolution=resolution,
        work_order=work_order,
        authority_ceiling=authority_ceiling,
        approval_floor=approval_floor,
        required_evidence_ids=required_evidence_ids,
    )


def find_product_md(repo_root: Path) -> Optional[Path]:
    """Return the first PRODUCT.md found in the canonical search order, or None."""
    for rel in _PRODUCT_MD_SEARCH_PATHS:
        candidate = repo_root / rel
        if candidate.exists():
            return candidate
    return None


def is_product_md_stale(content: str) -> bool:
    """Return True if PRODUCT.md content is considered stale.

    Stale means: shorter than PRODUCT_MD_MIN_CHARS (200 bytes) OR TODO dominance
    (more than 25% of the content consists of [TODO] markers).

    The byte length (UTF-8 encoded) is compared against the threshold so this
    gate agrees with /blueprint's check-product-md.sh which uses `wc -c` (bytes).
    """
    byte_len = len(content.encode("utf-8"))
    if byte_len < PRODUCT_MD_MIN_CHARS:
        return True
    todo_count = content.count("[TODO]")
    if todo_count == 0:
        return False
    # Rough dominance check: each [TODO] marker is 6 chars; if they make up
    # more than 25% of the total byte length, it is dominated by placeholders.
    todo_chars = todo_count * len("[TODO]")
    if todo_chars / byte_len > PRODUCT_MD_TODO_DOMINANCE_RATIO:
        return True
    return False


def check_product_md_for_dispatch(
    repo_root: Path,
    plan_path: str,
    stages: List[str],
) -> bool:
    """Re-check PRODUCT.md at dispatch time for executor=impeccable tasks.

    Returns True if the check passes and dispatch should proceed.
    Returns False and emits a <help> tag to stdout if PRODUCT.md is missing,
    unreadable, or stale, signalling the loop to pause.

    Args:
        repo_root: Absolute path to the repository root.
        plan_path: Relative plan path (for the evidence attribute).
        stages: The /impeccable stages this task will run (for the evidence attribute).
    """
    product_md = find_product_md(repo_root)
    if product_md is not None:
        try:
            content = product_md.read_text()
            if not is_product_md_stale(content):
                return True
            # Stale - fall through to <help> emission
            evidence_suffix = ""
        except (OSError, UnicodeDecodeError) as exc:
            evidence_suffix = f", read_error={type(exc).__name__}"
    else:
        evidence_suffix = ""
    # Missing, unreadable, or stale: emit <help> and block dispatch
    stages_str = ", ".join(stages)
    evidence = f"{plan_path}, stages: [{stages_str}]{evidence_suffix}"
    print(
        f'<help reason="missing-product-md" evidence="{evidence}">\n'
        f"PRODUCT.md required by /impeccable but missing or stale.\n"
        f"Run /impeccable teach, then resume target.\n"
        f"</help>",
        flush=True,
    )
    return False


# ---------------------------------------------------------------------------
# Impeccable stage loop - full-loop iteration ceiling (decision 5c)
# ---------------------------------------------------------------------------

class ImpeccableVerdict(Enum):
    """Two-tier verdict for the /impeccable stage loop (decision 5a from brief)."""
    SUCCESS = "SUCCESS"
    DONE_WITH_CONCERNS = "DONE_WITH_CONCERNS"
    FAILED = "FAILED"


@dataclass
class ImpeccableStageLoop:
    """Tracks the shared iteration budget for a full /impeccable stage loop.

    The max_iterations budget applies to the ENTIRE stage loop (craft -> critique ->
    polish -> harden -> audit -> ...), not per-stage. A single iterations_used counter
    increments on every stage invocation. When iterations_used >= max_iterations,
    the ceiling is reached and the loop must exit with the two-tier verdict.

    This is the canonical model per decision 5c of the frontend-executor-pipeline-
    awareness brief: "the operator's iteration ceiling applies to the full stage loop,
    not per-stage; the budget is total, not multiplied across stages."
    """

    max_iterations: int = IMPECCABLE_DEFAULT_MAX_ITERATIONS
    critique_target: int = IMPECCABLE_DEFAULT_CRITIQUE_TARGET
    critique_floor: int = IMPECCABLE_DEFAULT_CRITIQUE_FLOOR
    iterations_used: int = field(default=0, init=False)

    @property
    def ceiling_reached(self) -> bool:
        """Return True when the shared budget is exhausted."""
        return self.iterations_used >= self.max_iterations

    def can_dispatch(self) -> bool:
        """Return True when the next stage invocation is within budget."""
        return self.iterations_used < self.max_iterations

    def record_stage(self, stage: str) -> None:  # noqa: ARG002
        """Record one stage invocation against the shared budget."""
        self.iterations_used += 1

    def compute_verdict(self, final_score: int) -> ImpeccableVerdict:
        """Compute the two-tier exit verdict from the final critique score.

        Score >= critique_target  -> SUCCESS
        Score <  critique_floor   -> FAILED
        Otherwise (band)          -> DONE_WITH_CONCERNS

        Per decision 5a: the ceiling exit is NOT a hard FAILED reflex; the
        score determines which tier applies.
        """
        if final_score >= self.critique_target:
            return ImpeccableVerdict.SUCCESS
        if final_score < self.critique_floor:
            return ImpeccableVerdict.FAILED
        return ImpeccableVerdict.DONE_WITH_CONCERNS
HIDDEN_SHARED_OUTPUT_ROOTS = (
    ".fno/",
    ".codex/agents/",
    ".gemini/agents/",
    "docs/",
    "internal/",
)


def _parse_scalar(value: str):
    value = value.strip()
    if value in ("true", "false"):
        return value == "true"
    if value.startswith("[") and value.endswith("]"):
        inner = value[1:-1].strip()
        if not inner:
            return []
        return [item.strip().strip('"').strip("'") for item in inner.split(",")]
    if re.fullmatch(r"-?\d+", value):
        return int(value)
    return value.strip('"').strip("'")


# The --harness/--provider value names the invoking HARNESS (which CLI binary
# runs the work), not the model vendor. These are the values detect_provider()
# can return and that an explicit argument may name.
_KNOWN_HARNESSES = ("claude", "codex", "gemini")


def detect_provider(env: Optional[dict] = None) -> str:
    """Sniff the invoking harness from environment variables.

    Compatibility FALLBACK only. Prefer an explicit ``--harness``/``--provider``
    argument via :func:`resolve_invoking_harness`, which surfaces this sniff as
    a fallback source so a Codex session that has not exported
    ``CODEX_PLUGIN_ROOT`` cannot silently redirect its waves to Claude
    subagents. Returns a harness (``claude`` | ``codex`` | ``gemini``), not a
    model vendor.
    """
    environ = os.environ if env is None else env
    if environ.get("CODEX_PLUGIN_ROOT"):
        return "codex"
    if environ.get("GEMINI_PROJECT_DIR"):
        return "gemini"
    return "claude"


def resolve_invoking_harness(
    explicit: Optional[str] = None,
    env: Optional[dict] = None,
) -> tuple[str, str]:
    """Resolve the invoking harness, explicit-first, env-sniff as a surfaced fallback.

    Returns ``(harness, source)`` where ``source`` is ``"explicit"`` or
    ``"env-fallback"``. An explicit value wins and is validated against the
    known harnesses; the env sniff runs only when no explicit value was given,
    and its result is reported as a fallback rather than reading as a deliberate
    choice. The value is a harness, never a model vendor.
    """
    if explicit is not None and explicit.strip():
        h = explicit.strip().lower()
        if h not in _KNOWN_HARNESSES:
            raise ValueError(
                f"unknown harness {explicit!r}; expected one of "
                f"{', '.join(_KNOWN_HARNESSES)}"
            )
        return h, "explicit"
    return detect_provider(env=env), "env-fallback"


def load_project_constraints() -> List[str]:
    """Load project constraints from settings.yaml (local > global)."""
    for path in [
        Path(".fno/settings.yaml"),
        Path.home() / ".claude" / "fno" / "settings.yaml",
    ]:
        if path.exists():
            try:
                constraints: List[str] = []
                in_constraints = False
                for line in path.read_text().splitlines():
                    if line.startswith("  constraints:"):
                        in_constraints = True
                        continue
                    if in_constraints and re.match(r"^  [a-zA-Z_]+:", line):
                        break
                    if in_constraints and re.match(r"^    - ", line):
                        constraints.append(line.split("-", 1)[1].strip().strip('"'))
                return constraints
            except OSError as e:
                print(f"Warning: Failed to load constraints from {path}: {e}", file=sys.stderr)
                continue
    return []


def format_constraints_section(constraints: List[str]) -> str:
    """Format constraints as markdown section for agent prompt injection."""
    if not constraints:
        return ""
    lines = ["", "## Project Constraints", ""]
    for c in constraints:
        lines.append(f"- {c}")
    return "\n".join(lines)


def get_project_constraints_section() -> str:
    """Get formatted constraints section for agent prompt injection."""
    return format_constraints_section(load_project_constraints())


# Domain routing - keywords that determine which domain checklist to inject
# All tasks use the "archer" agent; domain determines CONTEXT.md content
_DOMAIN_KEYWORDS: Dict[str, List[str]] = {
    "frontend": [
        "frontend", "tanstack", "react", "ui", "component", "css", "tailwind",
        "nextjs", "next.js", "vite",
    ],
    "backend": [
        "backend", "api", "supabase", "database", "auth", "server",
        "newsletter", "email", "graphql", "trpc",
    ],
    "devops": [
        "devops", "docker", "ci", "ci/cd", "cicd", "deploy", "terraform",
        "kubernetes", "k8s", "motia", "workflow", "orchestration", "github actions",
    ],
    "data": [
        "etl", "pipeline", "transform", "data", "analytics", "regulation",
        "compliance", "extraction", "parsing", "inspection",
    ],
}

# Build flat keyword->domain map
DOMAIN_MAP: Dict[str, str] = {
    keyword: domain
    for domain, keywords in _DOMAIN_KEYWORDS.items()
    for keyword in keywords
}

# Legacy alias: agent routing maps to target for all domains
# Kept for backward compatibility with --agent CLI flag
AGENT_MAP: Dict[str, str] = {
    keyword: "archer"
    for domain, keywords in _DOMAIN_KEYWORDS.items()
    for keyword in keywords
}


@dataclass
class Wave:
    number: int
    mode: str  # 'sequential' | 'parallel'
    tasks: List[str]
    reason: str
    # Blueprint's per-wave band ('low' | 'medium' | 'high'); '' when the plan
    # predates the band. A pulling worker filters on it; models stay in config.
    difficulty: str = ""


@dataclass
class Task:
    """Represents a single task with metadata for agent routing."""
    id: str
    description: str
    tags: List[str] = field(default_factory=list)


@dataclass
class ExecutionStrategy:
    execution_mode: str  # 'sequential' | 'parallel' | 'mixed'
    waves: List[Wave]
    scope: str = "single-project"  # 'single-project' | 'cross-project'
    project_tasks: Dict[str, List[str]] = field(default_factory=dict)
    # Declared per-task edges (task id -> blocked_by list). A task with no
    # declared entry derives its blockers from the previous wave instead.
    blocked_by: Dict[str, List[str]] = field(default_factory=dict)
    # Canonical task surfaces parsed from the Execution Strategy task blocks.
    # The prose `### Task` / `**Files:**` scan misses plans that declare
    # ownership only here, which would read every task as unevaluated.
    task_surfaces: Dict[str, List[str]] = field(default_factory=dict)


def _extract_task_section(phase_file: Path, task_id: str) -> List[str]:
    section: List[str] = []
    in_task = False
    for line in phase_file.read_text().splitlines():
        if re.match(rf"^### Task {re.escape(task_id)}([^0-9]|$)", line):
            in_task = True
            section = []
            continue
        if in_task and line.startswith("### Task "):
            break
        if in_task:
            section.append(line)
    return section


def get_task_file_targets(plan_path: str, task_id: str) -> List[str]:
    """Return normalized file targets from a task's Files section."""
    targets: List[str] = []
    section = _extract_task_section(Path(plan_path), task_id)
    if not section:
        return targets

    collecting = False
    for raw_line in section:
        line = raw_line.strip()
        if not collecting and re.match(r"^(\*\*Files?:\*\*|Files?:|## Files?)", line):
            collecting = True
            continue
        if collecting and (
            line.startswith("**Acceptance Criteria")
            or line.startswith("Acceptance Criteria")
            or line.startswith("**Steps:")
            or line.startswith("Steps:")
            or line.startswith("---")
        ):
            break
        if collecting and line.startswith(("- ", "* ")):
            target = re.sub(r"^(Create|Modify|Update|Delete):\s*", "", line[2:]).strip()
            target = target.strip("`")
            if target:
                targets.append(target)
    return targets


def _task_targets(
    plan_path: str, task_id: str, surfaces: Optional[Dict[str, List[str]]]
) -> List[str]:
    """The task's file targets: canonical surfaces first, prose scan as fallback.

    A plan that declares ownership only through Execution Strategy `surface:`
    lists has no `### Task` prose to scan, so the prose scan alone would read
    it as unevaluated and partition away a real overlap.
    """
    if surfaces is not None:
        canonical = [str(s) for s in surfaces.get(task_id, [])]
        if canonical:
            return canonical
    return get_task_file_targets(plan_path, task_id)


def detect_hidden_output_conflicts(
    plan_path: str,
    task_ids: List[str],
    surfaces: Optional[Dict[str, List[str]]] = None,
) -> Dict[str, List[str]]:
    """Partition a wave's tasks by file overlap and shared-output roots.

    Returns the collision partition (``groups`` of overlapping task ids,
    ``unevaluated`` ids with no parseable file list) plus the two legacy
    conflict keys the ``--wave-decision`` printer still reports.
    """
    from fno.graph.collision import match_shared_root, partition

    by_file: Dict[str, List[str]] = {}
    by_root: Dict[str, List[str]] = {}
    file_conflicts: List[str] = []
    root_conflicts: List[str] = []
    items: List[tuple[str, set[str]]] = []

    for task_id in task_ids:
        targets = set(_task_targets(plan_path, task_id, surfaces))
        items.append((task_id, targets))
        for target in sorted(targets):
            owners = by_file.setdefault(target, [])
            owners.append(task_id)
            if len(owners) == 2:
                file_conflicts.append(target)

            shared_root = match_shared_root(target, HIDDEN_SHARED_OUTPUT_ROOTS)
            if shared_root:
                root_owners = by_root.setdefault(shared_root, [])
                if task_id not in root_owners:
                    root_owners.append(task_id)
                if len(root_owners) == 2:
                    root_conflicts.append(shared_root)

    groups, unevaluated = partition(items, shared_roots=HIDDEN_SHARED_OUTPUT_ROOTS)
    return {
        "groups": [sorted(group) for group in groups],
        "unevaluated": sorted(unevaluated),
        "file_conflicts": sorted(set(file_conflicts)),
        "shared_output_conflicts": sorted(set(root_conflicts)),
    }


def partition_edges(
    plan_path: str,
    wave: Wave,
    surfaces: Optional[Dict[str, List[str]]] = None,
) -> Dict[str, List[str]]:
    """Derived within-wave edges from the collision partition.

    Inside a multi-member group, task N blocks on task N-1 in wave order;
    every unevaluated task blocks on every evaluated task in the wave. The
    ``--ready`` query unions these with the declared ``blocked_by`` edges,
    so the overlapping tasks serialize without flipping the wave's mode.
    """
    from fno.graph.collision import partition

    items = [
        (task_id, set(_task_targets(plan_path, task_id, surfaces)))
        for task_id in wave.tasks
    ]
    groups, unevaluated = partition(items, shared_roots=HIDDEN_SHARED_OUTPUT_ROOTS)

    edges: Dict[str, List[str]] = {}
    for group in groups:
        ordered = [task_id for task_id in wave.tasks if task_id in group]
        for prev, cur in zip(ordered, ordered[1:]):
            edges.setdefault(cur, []).append(prev)
    evaluated = [task_id for task_id in wave.tasks if task_id not in unevaluated]
    for task_id in wave.tasks:
        if task_id in unevaluated and evaluated:
            edges[task_id] = list(evaluated)
    return edges


def apply_partition_edges(strategy: ExecutionStrategy, plan_path: str) -> None:
    """Union partition-derived edges into ``strategy.blocked_by`` in place.

    A declared list is kept and extended, never replaced. A task with no
    declared entry keeps its previous-wave inheritance as the base, so a
    derived edge can never float a task ahead of an incomplete earlier wave.
    """
    for wave in strategy.waves:
        if wave.mode != "parallel":
            continue
        for task_id, blockers in partition_edges(plan_path, wave, strategy.task_surfaces).items():
            if task_id in strategy.blocked_by:
                base = set(strategy.blocked_by[task_id])
            else:
                base = set()
                for pos, holder in enumerate(strategy.waves):
                    if task_id in holder.tasks:
                        if pos:
                            base |= set(strategy.waves[pos - 1].tasks)
                        break
            strategy.blocked_by[task_id] = sorted(base | set(blockers))


# Providers whose stable baseline cannot spawn concurrent Task-tool subagents,
# so a conflict-free parallel wave still downgrades to sequential main-thread
# dispatch. Claude and Codex support parallel subagents; Gemini's baseline is
# sequential (skills/execute/references/waves.md). This is the one provider fact the
# wave-mode resolver still needs after the static capability matrix was removed.
SEQUENTIAL_FALLBACK_PROVIDERS = frozenset({"gemini"})


def resolve_wave_execution_mode(
    wave: Wave,
    plan_path: str,
    provider: Optional[str] = None,
    surfaces: Optional[Dict[str, List[str]]] = None,
) -> Dict[str, object]:
    """Resolve effective wave mode from requested mode and hidden file/shared-output conflicts."""
    resolved_provider, harness_source = resolve_invoking_harness(provider)
    decision: Dict[str, object] = {
        "provider": resolved_provider,
        "harness_source": harness_source,
        "requested_mode": wave.mode,
        "effective_mode": wave.mode,
        "dispatch": "main-thread",
        "reason": "Wave is sequential by plan",
        "conflicts": {
            "groups": [],
            "unevaluated": [],
            "file_conflicts": [],
            "shared_output_conflicts": [],
        },
    }

    if wave.mode != "parallel":
        return decision

    conflicts = detect_hidden_output_conflicts(plan_path, wave.tasks, surfaces)
    decision["conflicts"] = conflicts

    if resolved_provider in SEQUENTIAL_FALLBACK_PROVIDERS:
        decision["effective_mode"] = "sequential"
        decision["reason"] = (
            f"Parallel wave downgraded: {resolved_provider} runs sequential "
            "main-thread (no concurrent Task-tool subagents)"
        )
        return decision

    decision["dispatch"] = "native-subagents"
    overlapping = any(len(group) > 1 for group in conflicts["groups"]) or bool(
        conflicts["unevaluated"]
    )
    if overlapping:
        # The wave stays parallel: partition edges hold the overlapping tasks
        # back while the ready set dispatches concurrently.
        decision["reason"] = (
            "Parallel wave dispatches the ready set; partition edges serialize overlapping tasks"
        )
    else:
        decision["reason"] = "Parallel wave has no file or shared-output conflicts"
    return decision


def parse_execution_strategy(index_path: str) -> Optional[ExecutionStrategy]:
    """Compatibility entrypoint backed by the canonical plan loader."""
    return load_plan_strategy(index_path)


def get_completed_tasks_from_state(state_path: str) -> List[str]:
    """Parse completed tasks from STATE.md"""
    path = Path(state_path)
    if not path.exists():
        return []

    content = path.read_text()
    completed = []

    # Match lines like "- [x] 1.1: Task name" or "- [x] 01: Task name"
    # Supports: 1.1, 2.1, 01, 02, 02b, 02a, 02AB formats
    for match in re.finditer(r'- \[x\] ([\d.]+[a-zA-Z]*):', content):
        completed.append(match.group(1))

    return completed


def get_failed_tasks_from_state(state_path: str) -> List[str]:
    """Parse failed tasks from STATE.md"""
    path = Path(state_path)
    if not path.exists():
        return []

    content = path.read_text()
    failed = []

    # Match lines like "- [!] 2.2: Task name - FAILED" or "- [!] 02b: Task name - FAILED"
    # Supports: 1.1, 2.1, 01, 02, 02b, 02a, 02AB formats
    for match in re.finditer(r'- \[!\] ([\d.]+[a-zA-Z]*):.*FAILED', content):
        failed.append(match.group(1))

    return failed


def get_next_wave(strategy: ExecutionStrategy, completed_tasks: List[str]) -> Optional[Wave]:
    """Get the next wave to execute, as the wave holding the first ready task.

    Compatibility shim over ``ready_tasks``: per-task edges may float a later
    task ahead of an open sibling, so "next wave" is defined by readiness,
    not by first-wave-not-entirely-complete.
    """
    ready = ready_tasks(strategy, completed_tasks, [])
    if not ready:
        return None  # All waves complete (or nothing dispatchable)
    for wave in strategy.waves:
        if ready[0] in wave.tasks:
            return wave
    return None


def effective_blockers(strategy: ExecutionStrategy, task_id: str) -> Set[str]:
    """The tasks that must complete before ``task_id`` may start.

    A declared non-empty ``blocked_by`` wins outright; otherwise a task
    inherits every task id in the wave before its own (empty for the first
    wave), so a plan without per-task edges schedules exactly as the old
    whole-wave barrier did.
    """
    if task_id in strategy.blocked_by:
        return set(strategy.blocked_by[task_id])
    for pos, wave in enumerate(strategy.waves):
        if task_id in wave.tasks:
            if pos == 0:
                return set()
            return set(strategy.waves[pos - 1].tasks)
    return set()


def ready_tasks(
    strategy: ExecutionStrategy,
    completed: Collection[str],
    claimed: Collection[str],
) -> List[str]:
    """Tasks dispatchable now, in wave order then declaration order.

    A task is ready when it is neither completed nor claimed and every
    effective blocker is completed. This is the work-stealing query: list,
    pick an unowned entry, claim.
    """
    done = set(completed)
    busy = set(claimed)
    ready: List[str] = []
    for wave in strategy.waves:
        for task_id in wave.tasks:
            if task_id in done or task_id in busy:
                continue
            if effective_blockers(strategy, task_id) <= done:
                ready.append(task_id)
    return ready


def get_pending_tasks_in_wave(wave: Wave, completed_tasks: List[str]) -> List[str]:
    """Get tasks in a wave that haven't been completed yet"""
    return [task for task in wave.tasks if task not in completed_tasks]


def format_state_update(
    strategy: ExecutionStrategy,
    completed_tasks: List[str],
    plan_path: str,
    failed_tasks: Optional[List[str]] = None,
    task_descriptions: Optional[dict] = None
) -> str:
    """Format STATE.md update after wave completion.

    Args:
        strategy: The execution strategy with waves
        completed_tasks: List of completed task IDs
        plan_path: Path to the plan folder
        failed_tasks: List of failed task IDs
        task_descriptions: Optional dict mapping task IDs to descriptions
                          e.g. {"1.1": "Create database schema", "2.1": "Implement auth"}
    """
    failed_tasks = failed_tasks or []
    task_descriptions = task_descriptions or {}
    timestamp = datetime.now().isoformat()

    lines = [
        "# Session State",
        "",
        f"updated: {timestamp}",
        f"plan: {plan_path}",
        "",
        "## Wave Progress"
    ]

    for wave in strategy.waves:
        all_complete = all(t in completed_tasks for t in wave.tasks)
        any_started = any(t in completed_tasks or t in failed_tasks for t in wave.tasks)

        if all_complete:
            status = "COMPLETE"
            marker = "[x]"
        elif any_started:
            status = "IN PROGRESS"
            marker = "[ ]"
        else:
            status = "PENDING"
            marker = "[ ]"

        lines.append(f"- {marker} Wave {wave.number}: {wave.reason} - {status}")

    lines.extend(["", "## Task Status"])

    # Collect all tasks from all waves
    for wave in strategy.waves:
        for task in wave.tasks:
            # Use task description if available, otherwise use task ID as description
            description = task_descriptions.get(task, f"Task {task}")
            if task in completed_tasks:
                marker = "[x]"
                suffix = ""
            elif task in failed_tasks:
                marker = "[!]"
                suffix = " - FAILED"
            else:
                marker = "[ ]"
                suffix = ""
            lines.append(f"- {marker} {task}: {description}{suffix}")

    return "\n".join(lines)


def print_wave_summary(
    strategy: ExecutionStrategy,
    plan_path: Optional[str] = None,
    provider: Optional[str] = None,
) -> None:
    """Print a summary of the execution strategy"""
    harness, source = resolve_invoking_harness(provider)
    print(f"Execution mode: {strategy.execution_mode}")
    print(f"Invoking harness: {harness} ({source})")
    print(f"Total waves: {len(strategy.waves)}")
    print()

    for wave in strategy.waves:
        task_count = len(wave.tasks)
        print(f"Wave {wave.number} ({wave.mode}):")
        print(f"  Tasks: {', '.join(wave.tasks)}")
        print(f"  Reason: {wave.reason}")
        if wave.mode == 'parallel':
            print(f"  Parallelism: {task_count} concurrent")
            if plan_path:
                decision = resolve_wave_execution_mode(wave, plan_path, provider)
                print(f"  Effective mode: {decision['effective_mode']}")
                print(f"  Dispatch: {decision['dispatch']}")
                print(f"  Decision: {decision['reason']}")
        print()


def determine_domain(task: Task) -> str:
    """
    Determine the task domain based on tags and description keywords.

    The domain determines which checklist is injected into CONTEXT.md
    before spawning target. All tasks use the same agent (target).

    Priority:
    1. Check explicit tags first
    2. Check description keywords
    3. Fall back to 'general'

    Returns:
        Domain string: 'frontend', 'backend', 'devops', 'data', or 'general'
    """
    # Check tags first (highest priority)
    if task.tags:
        for tag in task.tags:
            tag_lower = tag.lower()
            if tag_lower in DOMAIN_MAP:
                return DOMAIN_MAP[tag_lower]

    # Check description keywords - sort by length (longer = more specific = higher priority)
    desc_lower = task.description.lower()
    sorted_keywords = sorted(DOMAIN_MAP.keys(), key=len, reverse=True)
    for keyword in sorted_keywords:
        pattern = r'\b' + re.escape(keyword) + r'\b'
        if re.search(pattern, desc_lower):
            return DOMAIN_MAP[keyword]

    return "general"


def determine_agent_type(task: Task) -> str:
    """Determine agent type — always returns 'archer'."""
    return "archer"


def determine_agent_type_from_description(description: str, tags: Optional[List[str]] = None) -> str:
    """Convenience function to determine agent type from a description string."""
    task = Task(id="", description=description, tags=tags or [])
    return determine_agent_type(task)


def get_agent_info(agent_type: str) -> dict:
    """Get information about a specific agent type.

    All tasks now route to archer. Domain metadata is kept for
    logging, progress display, and CONTEXT.md checklist injection.
    """
    domain_metadata = {
        "general": ("cyan", "TDD task executor"),
        "frontend": ("green", "Frontend task (React, TanStack, Tailwind)"),
        "backend": ("blue", "Backend task (API, database, auth)"),
        "devops": ("orange", "DevOps task (Docker, CI/CD, Terraform)"),
        "data": ("purple", "Data engineering task (ETL, pipelines, parsing)"),
    }

    # Map legacy archer names to domain
    domain = agent_type
    if agent_type.startswith("archer-"):
        domain = agent_type[6:]  # strip "archer-" prefix
    elif agent_type in ("doing", "operator", "target", "archer"):
        domain = "general"

    if domain not in domain_metadata:
        domain = "general"

    color, description = domain_metadata[domain]
    return {
        "name": "archer",
        "domain": domain,
        "color": color,
        "description": description,
    }


@dataclass
class TaskResult:
    """Result from an execution agent (archer/impeccable/...)."""
    status: str  # one of VALID_STATUSES
    task_id: str
    commit: Optional[str] = None
    summary: Optional[str] = None
    error: Optional[str] = None
    reason: Optional[str] = None
    unblocks_after: Optional[str] = None
    concerns: Optional[str] = None
    # True when validated from a schema-enforced structured block (the claude
    # path); False when parsed from the RESULT: text grammar (codex/gemini
    # fallback). Either way the status is enum-validated.
    structured: bool = False


# The execution-agent return-contract status enum (AGENTS.md "Return Contract").
# A status outside this set is REJECTED, never coerced: the parse layer fails
# CLOSED so a model that appends prose or invents a status yields no false
# success (ab-1394e797: "text output-format conventions fail open; schema
# validation happens at the tool-call layer").
VALID_STATUSES = ("SUCCESS", "DONE_WITH_CONCERNS", "FAILED", "BLOCKED")

# Canonical worker-facing return-contract instruction (increase-consistency.md).
# Derived from VALID_STATUSES so the enumerated statuses can NEVER drift from the
# parser. Three levers that raise well-formed-output rate without touching the
# parser seam: (1) one exact well-formed example of the fenced ```json block,
# (2) the block must be the LAST thing in the reply, (3) the four statuses
# enumerated inline. True assistant-prefill is unreachable through headless CLI
# dispatch, so example-plus-constraint is the available lever. The example here
# is round-tripped through parse_task_result in tests: what we tell workers to
# emit is exactly what the parser accepts.
RETURN_CONTRACT_INSTRUCTION = f"""\
Report your result as a single fenced JSON block that is the LAST thing in your
reply (nothing after the closing fence). `result` MUST be exactly one of
{" | ".join(VALID_STATUSES)}. Emit exactly this shape:

```json
{{"result": "SUCCESS", "task": "2.1", "commit": "abc1234", "summary": "one line"}}
```

`result` and `task` are required. `commit`/`summary` are optional on SUCCESS;
use `error` on FAILED and `reason`/`unblocks_after` on BLOCKED. Put no prose
inside the block and nothing after it."""

# Recognized contract field keys. In the text-grammar fallback a line is read as
# a field ONLY when its key is one of these, so prose the model appends
# ("Note: ...", "I fixed the bug: ...") cannot be absorbed as a field - the
# fail-open hole that let a stray colon line pollute the result.
_CONTRACT_KEYS = frozenset(
    {"RESULT", "TASK", "COMMIT", "SUMMARY", "ERROR", "REASON",
     "UNBLOCKS_AFTER", "CONCERNS"}
)

# Structured envelopes the claude path emits: a fenced ```json object or a
# <result>{...}</result> tag carrying the contract as JSON.
_JSON_FENCE_RE = re.compile(r"```(?:json)?\s*(\{.*?\})\s*```", re.DOTALL)
_RESULT_TAG_RE = re.compile(r"<result>\s*(\{.*?\})\s*</result>", re.DOTALL | re.IGNORECASE)


def _build_task_result(data: dict, *, structured: bool) -> Optional[TaskResult]:
    """Validate a KEY->value map against the contract; None if invalid (fail closed).

    The status must be EXACTLY one of ``VALID_STATUSES`` (after stripping
    surrounding whitespace/punctuation/quotes, but NOT trailing words - so
    "SUCCESS." validates while "SUCCESS but it failed" does not), and ``TASK``
    must be non-empty. Every other field is optional and normalized to None when
    blank.
    """
    status = str(data.get("RESULT", "")).strip().strip(".`*'\"").strip().upper()
    task_id = str(data.get("TASK", "")).strip()
    if status not in VALID_STATUSES or not task_id:
        return None

    def _opt(key: str) -> Optional[str]:
        val = data.get(key)
        if val is None:
            return None
        text = str(val).strip()
        return text or None

    return TaskResult(
        status=status,
        task_id=task_id,
        commit=_opt("COMMIT"),
        summary=_opt("SUMMARY"),
        error=_opt("ERROR"),
        reason=_opt("REASON"),
        unblocks_after=_opt("UNBLOCKS_AFTER"),
        concerns=_opt("CONCERNS"),
        structured=structured,
    )


def parse_structured_result(output: str) -> Optional[TaskResult]:
    """Parse a schema-enforced structured return block (the claude path).

    Looks for a fenced ```json object or a ``<result>{...}</result>`` envelope and
    validates it against the contract via ``_build_task_result``. Keys are
    upper-cased, so ``{"result": "success", "task": "1.2"}`` and
    ``{"RESULT": ...}`` both validate.

    Returns None when no structured block is present (the caller falls back to
    the text grammar) OR when a block is present but fails validation - a
    malformed structured block is NEVER silently accepted (fail closed).
    """
    if not output:
        return None
    match = _JSON_FENCE_RE.search(output) or _RESULT_TAG_RE.search(output)
    if not match:
        return None
    try:
        obj = json.loads(match.group(1))
    except (json.JSONDecodeError, ValueError):
        return None
    if not isinstance(obj, dict):
        return None
    data = {str(k).strip().upper(): v for k, v in obj.items()}
    return _build_task_result(data, structured=True)


def parse_task_result(output: str) -> Optional[TaskResult]:
    """Parse an execution agent's structured return, schema-first and fail-closed.

    Resolution order (ab-1394e797 - schema over the RESULT: stdout grammar):

    1. A schema-enforced structured block (```json or ``<result>``) is preferred,
       the claude dispatch path. It is validated against the contract; a
       malformed block is rejected, never coerced.
    2. Otherwise the ``RESULT:`` text grammar - the codex/gemini fallback. Only
       known contract keys are read as fields (so appended prose cannot pollute
       the result), the FIRST occurrence of each key wins (a later stray
       ``RESULT:`` in prose cannot hijack it), and the status must be EXACTLY one
       of ``SUCCESS|DONE_WITH_CONCERNS|FAILED|BLOCKED`` or the parse fails - no
       ``UNKNOWN`` false-success.

    A structured envelope, once emitted, is authoritative: if one is present but
    fails validation the parse fails CLOSED (returns None) rather than scraping
    the surrounding prose, which could pick a stray ``RESULT:`` line out of the
    agent's narration. The text grammar runs only when NO structured block exists.

    Returns None when neither path yields a valid, complete result.
    """
    if not output:
        return None

    # A structured block is authoritative when present (claude path) - validate
    # it and do not fall back on failure.
    if _JSON_FENCE_RE.search(output) or _RESULT_TAG_RE.search(output):
        return parse_structured_result(output)

    data: dict = {}
    for line in output.strip().split("\n"):
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        key = key.strip().upper()
        # Known keys only, first-occurrence-wins: ignore appended prose lines and
        # refuse to let a later stray contract line override the real one.
        if key in _CONTRACT_KEYS and key not in data:
            data[key] = value.strip()

    return _build_task_result(data, structured=False)


# Barrier vocabulary: a worker return classified for the deterministic fan-in
# count in fno.events.verify_child_promise.tally_fan_in. The two files connect
# through this (task_id, kind) tuple, NOT a cross-package import - orchestrator
# stays free of `fno.*` imports so `python skills/execute/orchestrator.py --help`
# runs under the ambient python that lacks the fno package.
#   kind "completed"       -> SUCCESS | DONE_WITH_CONCERNS
#   kind "failed"          -> FAILED | BLOCKED (unresolved; the barrier must not release)
#   kind "runtime_failed"  -> runtime-observed death (signal/error/context-limit/
#                             refusal), no usable claim to attribute
#   kind "unknown_terminal"-> ran and emitted retained partial output that must
#                             never surface as an answer
#   kind "no_output"       -> nothing observed (the instrument never ran, or the
#                             pipeline lost it)
_COMPLETED_STATUSES = frozenset({"SUCCESS", "DONE_WITH_CONCERNS"})

# The classifier's return kind, mirroring fno.events' FanInKind without the
# fno.* import this module must avoid (it runs under the ambient python). A
# local Literal (not a bare str) so a typo'd kind fails type-check instead of
# silently counting as malformed at the tally.
ClassifiedKind = Literal[
    "completed",
    "failed",
    "runtime_failed",
    "unknown_terminal",
    "no_output",
]

# Runtime-observed worker terminal vocabulary (x-1862): the process-level half
# of a worker return, independent of anything the worker wrote about itself.
# The dispatcher encodes process death as an exit code (a signal death arrives
# as 128+N); ``runtime_terminal_from_exit_code`` is the producer-side mapping
# of that encoding onto this vocabulary, so a barrier consumer hands the pair
# ``classify_worker_return(output, runtime_terminal_from_exit_code(rc))`` to
# the classifier. None means "no observation available" and never fabricates a
# verdict; an unrecognized non-None value is failure-class (an unknown
# terminal reason is a failure, never partial output as success -
# deepseek-harness stopReasonError precedent).
_RUNTIME_TERMINALS = frozenset({"completed", "error", "signal", "context_limit", "refusal"})
_RUNTIME_FAILURE_TERMINALS = frozenset({"error", "signal", "context_limit", "refusal"})


def runtime_terminal_from_exit_code(exit_code: int) -> str:
    """Map a dispatch exit status onto the runtime-terminal vocabulary.

    The shell convention the dispatcher already writes into ``node_failed``
    events: 0 is a clean exit, 128+N is death by signal N, anything else is a
    plain error. A negative code is Python's ``subprocess`` spelling of a
    signal death, so it maps to ``"signal"`` too. Harness-level terminals
    (``context_limit``, ``refusal``) have no exit-code spelling; a consumer
    that can observe them passes them directly instead of this function.
    """
    if exit_code == 0:
        return "completed"
    if exit_code > 128 or exit_code < 0:
        return "signal"
    return "error"


def classify_worker_return(
    output: str,
    runtime_terminal: Optional[str] = None,
) -> tuple[Optional[str], Optional[ClassifiedKind]]:
    """Validate a worker return against the canonical contract and classify it.

    Two independent inputs, kept separate on purpose (x-1862): ``output`` is
    the worker's self-report (the CLAIM); ``runtime_terminal`` is what the
    runtime OBSERVED about the process (``"completed"``, ``"error"``,
    ``"signal"`` for a 128+N death, ``"context_limit"``, ``"refusal"``). A
    failure-class observation outranks a completing claim - a self-report
    cannot certify a process the runtime watched die - and an unrecognized
    observation reads as failure, never success. ``None`` (default) means no
    observation was available and the claim is graded alone.

    Reuses ``parse_task_result`` (the single source of the status enum) so a
    malformed or invalid-status return can never pose as complete. Returns a
    ``(task_id, kind)`` pair for ``tally_fan_in``. ``(None, None)`` is never
    returned: a shapeless answer is exactly the absence-versus-outcome
    ambiguity this classifier exists to refuse.
    """
    observed_failure = runtime_terminal in _RUNTIME_FAILURE_TERMINALS or (
        runtime_terminal is not None and runtime_terminal not in _RUNTIME_TERMINALS
    )
    result = parse_task_result(output)
    if result is None:
        if observed_failure:
            return None, "runtime_failed"
        if not output or not output.strip():
            return None, "no_output"
        return None, "unknown_terminal"
    if observed_failure and result.status in _COMPLETED_STATUSES:
        return result.task_id, "failed"
    kind: ClassifiedKind = "completed" if result.status in _COMPLETED_STATUSES else "failed"
    return result.task_id, kind


def emit_status_event(
    event_type: str,
    *,
    run: str = "",
    node: str = "",
    task: str = "",
    outcome: str = "",
    data: Optional[Dict] = None,
) -> bool:
    """Emit an x-dbaf status-breakpoint event by shelling ``fno doctor event emit``.

    Non-fatal by contract: any failure logs one stderr note and returns False;
    it never raises, so a boundary emit can never fail the task or the run.
    Skills shell to the installed CLI (never import repo code), matching
    resolve-executor.sh. Work coordinates fall back to the manifest inside the
    emit CLI when the flags are empty.
    """
    argv = [
        "fno",
        "doctor",
        "event",
        "emit",
        "-t",
        event_type,
        "-d",
        json.dumps(data or {}),
    ]
    for flag, val in (("--run", run), ("--node", node), ("--task", task), ("--outcome", outcome)):
        if val:
            argv += [flag, val]
    return _shell_fno(argv, f"{event_type} emit")


def _shell_fno_result(argv: List[str], what: str) -> Optional[subprocess.CompletedProcess]:
    """Run one best-effort ``fno`` subprocess, returning it or None.

    The one runner for every non-fatal boundary shell (the event emit, the
    task-claim settle, the --ready task-row read): missing fno, a raise, and
    a non-zero exit each log one stderr note and return None. Never raises,
    so a boundary side effect can never fail the task or the run.
    """
    # 15s bounds an append-only event emit. The task-claim settle is a full
    # graph mutation behind the fleet-wide flock (recompute, JSON write,
    # sha256 sidecar, whole-board render), and a SIGKILL mid-settle strands a
    # pid-anchored claim that never goes stale, so the row reads peer-held for
    # the rest of the run. Give a settle the longer bound.
    timeout = 120 if "task" in argv[:4] else 15
    try:
        result = subprocess.run(argv, check=False, capture_output=True, timeout=timeout)
    except FileNotFoundError:
        print(f"orchestrator: note: fno unavailable, skipped {what}", file=sys.stderr)
        return None
    except Exception as exc:  # noqa: BLE001 - a boundary side effect never wedges the run
        print(f"orchestrator: note: {what} failed (non-fatal): {exc}", file=sys.stderr)
        return None
    if result.returncode != 0:
        print(
            f"orchestrator: note: {what} rejected (non-fatal): "
            f"{result.stderr.decode('utf-8', 'replace').strip()}",
            file=sys.stderr,
        )
        return None
    return result


def _shell_fno(argv: List[str], what: str) -> bool:
    """Boolean view of :func:`_shell_fno_result` for fire-and-forget callers."""
    return _shell_fno_result(argv, what) is not None


def _ambient_session_ids() -> Set[str]:
    """Return full session ids this orchestrator can identify as its own."""
    return {
        value.strip()
        for name in (
            "TARGET_SESSION_ID",
            "FNO_HARNESS_SESSION_ID",
            "CODEX_THREAD_ID",
            "CODEX_SESSION_ID",
            "CLAUDE_CODE_SESSION_ID",
            "GEMINI_SESSION_ID",
            "OPENCODE_SESSION_ID",
        )
        if (value := os.environ.get(name, "").strip())
    }


def _task_claim_state(node_id: str, task_id: str) -> tuple[Optional[bool], Optional[str]]:
    """Read one task claim as ``(live, holder)`` without mutating state.

    ``False`` means the claim is free or stale, ``True`` means a live holder
    exists, and ``None`` means the claim could not be read. Unknown is kept
    fail-closed by callers so an unreadable claim never becomes dispatchable.
    """
    result = _shell_fno_result(
        ["fno", "agents", "claim", "status", f"task:{node_id}:{task_id}", "--json"],
        f"task claim read for {node_id}/{task_id}",
    )
    if result is None:
        return None, None
    try:
        payload = json.loads(result.stdout.decode("utf-8", "replace"))
    except ValueError:
        print(
            f"orchestrator: note: unreadable task claim for {node_id}/{task_id}; "
            "suppressing dispatch",
            file=sys.stderr,
        )
        return None, None
    if not isinstance(payload, dict):
        print(
            f"orchestrator: note: malformed task claim for {node_id}/{task_id}; "
            "suppressing dispatch",
            file=sys.stderr,
        )
        return None, None
    state = payload.get("state")
    if state in {"live", "suspect"}:
        return True, str(payload.get("holder") or "")
    if state in {"free", "stale", "expired", "dead"}:
        return False, str(payload.get("holder") or "")
    print(
        f"orchestrator: note: unknown task claim state {state!r} for "
        f"{node_id}/{task_id}; suppressing dispatch",
        file=sys.stderr,
    )
    return None, str(payload.get("holder") or "")


#: Boundary outcomes that release the claim as done.
_TERMINAL_OUTCOMES = ("SUCCESS", "DONE_WITH_CONCERNS")
#: Boundary outcomes that give the task back to pending. NOT "": the `blocked`
#: branch hardcodes "FAILED", so an empty outcome can only arrive from a
#: `task_done` whose optional --outcome was omitted - giving a task that just
#: committed back to pending, for a peer to claim and re-run.
_GIVEBACK_OUTCOMES = ("FAILED", "BLOCKED")


def release_task_claim_at_boundary(node: str, task: str, outcome: str) -> bool:
    """Settle the task claim at a task boundary (x-09d7 group 3).

    ``done`` after a terminal outcome, ``pending`` (the holder-only give-back)
    after ``blocked``/``FAILED``, so the next ready worker can pick the task
    up. The outcome vocabulary is CLOSED: an unrecognized spelling
    (``success``, ``PARTIAL``) settles NOTHING and says so - mapping strays to
    the give-back would release a finished task's claim mid-flight. Non-fatal
    by the same contract as the emit itself: one stderr note and False on any
    failure, never an exception. A give-back by a caller that is not the
    holder is refused (exit 3) by the verb; that refusal lands here as the
    same non-fatal note.
    """
    if outcome in _TERMINAL_OUTCOMES:
        status_verb = "done"
    elif outcome in _GIVEBACK_OUTCOMES:
        status_verb = "pending"
    else:
        print(
            f"orchestrator: note: unknown task outcome {outcome!r}; task claim "
            "not settled",
            file=sys.stderr,
        )
        return False
    argv = ["fno", "backlog", "task", "update", node, task, "--status", status_verb]
    return _shell_fno(argv, "task claim settle")


def _manifest_path(state_path: str) -> Path:
    """``state_path`` if it is absolute or already resolves from cwd, else the
    first hit walking up to the repo root.

    The emit CLI resolves its own ``.fno`` through ``resolve_repo_root()``. A
    bare relative default only agrees with it when cwd IS the root, so from a
    subdirectory the emit still stamped the node while the settle read nothing
    and skipped - leaving the row in_progress under a claim nothing releases,
    which every later pass then reads as peer-held. This module is stdlib-only
    by design (it shells out to fno), so the walk stands in for the import: it
    honors ``FNO_REPO_ROOT`` (the resolver's first tier) and is bounded by the
    repo root, because a walk that is not bounded finds a STRANGER's manifest.
    """
    given = Path(state_path)
    if given.is_absolute() or given.exists():
        return given
    env = os.environ.get("FNO_REPO_ROOT")
    if env:
        candidate = Path(env) / given
        if candidate.exists():
            return candidate
        # Fall through rather than give up: resolve_repo_root only WARNS on a
        # foreign root, so a stale exported FNO_REPO_ROOT (canonical checkout,
        # session in a worktree) would otherwise blank every boundary and leak
        # every task claim for the run.
    here = Path.cwd().resolve()
    # Find the root FIRST, then search only within it. Searching on the way up
    # and stopping at the root afterwards climbs to the filesystem root when
    # there is no repo at all, and the first `.fno/target-state.md` it meets
    # there belongs to somebody else: task ids like "1.1" exist in nearly every
    # plan, so the settle would then land on an unrelated node.
    root = next((d for d in (here, *here.parents) if (d / ".git").exists()), None)
    if root is None:
        return given
    for parent in (here, *here.parents):
        candidate = parent / given
        if candidate.exists():
            return candidate
        if parent == root:
            break
    return given


def manifest_graph_node_id(state_path: str = ".fno/target-state.md") -> str:
    """The bound node id from the session manifest, or ``""``.

    The same best-effort substring scan the emit CLI's envelope fallback uses
    (work coordinates fall back to the manifest when the flags are empty), so
    the documented bare ``--emit-boundary task_done --task N.M`` (no --node)
    still settles the task claim. A missing or unreadable manifest yields ""
    and the settle is skipped.
    """
    try:
        text = _manifest_path(state_path).read_text(encoding="utf-8")
    except (OSError, ValueError):
        # UnicodeDecodeError is a ValueError and escapes a bare OSError catch,
        # so one non-UTF-8 byte in the manifest would raise out of the
        # --emit-boundary handler that never fails the run.
        return ""
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("graph_node_id:"):
            val = stripped[len("graph_node_id:") :].strip().strip('"').strip()
            if val and val != "null":
                return val
    return ""


def get_blocked_tasks_from_state(state_path: str) -> List[str]:
    """Parse blocked tasks from STATE.md"""
    path = Path(state_path)
    if not path.exists():
        return []

    content = path.read_text()
    blocked = []

    # Match lines like "- [B] 2.2: Task name - BLOCKED"
    for match in re.finditer(r'- \[B\] ([\d.]+[a-zA-Z]*):.*BLOCKED', content):
        blocked.append(match.group(1))

    return blocked


def format_blocked_status(result: TaskResult) -> str:
    """Format a BLOCKED result for display/logging."""
    lines = [
        "─" * 50,
        f"⚠️  TASK BLOCKED: {result.task_id}",
        "─" * 50,
        "",
        f"Reason: {result.reason or 'Unknown'}",
    ]

    if result.unblocks_after:
        lines.extend([
            "",
            "To unblock, the following must happen:",
            f"  → {result.unblocks_after}",
        ])

    lines.extend([
        "",
        "Options:",
        "  1. Resolve the blocker and run `/target resume`",
        "  2. Skip this task and continue with `--skip {task_id}`",
        "  3. Cancel the pipeline with `/target cancel`",
        "─" * 50,
    ])

    return "\n".join(lines)


def handle_blocked_task(
    result: TaskResult,
    state_path: str = ".fno/STATE.md"
) -> None:
    """
    Handle a BLOCKED task result by updating state and providing guidance.

    Args:
        result: The TaskResult with status=BLOCKED
        state_path: Path to the state file
    """
    # Print the blocked status
    print(format_blocked_status(result))

    # Update state file to mark task as blocked
    path = Path(state_path)
    if path.exists():
        content = path.read_text()
        # Replace the task status marker from [ ] to [B]
        pattern = rf'- \[ \] {re.escape(result.task_id)}:'
        replacement = f'- [B] {result.task_id}:'
        updated = re.sub(pattern, replacement, content)

        # Add blocked reason if not already present
        if result.reason and 'BLOCKED' not in updated:
            updated = updated.replace(
                f'- [B] {result.task_id}:',
                f'- [B] {result.task_id}: BLOCKED - {result.reason}'
            )

        path.write_text(updated)


# Self-contained frontmatter-schema check. Duplicated (not shared) with
# skills/blueprint: driver skills are CI-enforced self-contained and cannot
# cross-import. Validation runs under a pydantic-capable python because /execute's
# ambient python3 lacks pydantic (same reason finalize.rs shells the cli venv).
_VALIDATE_SNIPPET = r"""
import json, sys
from pydantic import ValidationError
from fno.plan.schema import PlanFrontmatter
fm = json.load(sys.stdin)
try:
    PlanFrontmatter.model_validate(fm)
except ValidationError as e:
    # Block only on STRUCTURAL corruption (a bad size, a garbage timestamp).
    # A missing required field is tolerated (a plan binds its node later), and
    # a drifted-but-recognizable `status` (planned/designed/superseded/...) is
    # NOT a /execute concern: `fno do plan reconcile-status` normalizes it, and /execute
    # executes the Execution Strategy section, which does not depend on the
    # frontmatter status. Blocking /execute on status drift would refuse ~5% of real
    # plans that run fine today.
    present = [
        x for x in e.errors()
        if x["type"] != "missing" and (x["loc"][:1] != ("status",))
    ]
    for x in present:
        loc = ".".join(str(p) for p in x["loc"]) or "<root>"
        print(f"  {loc}: {x['msg']} (got {x.get('input')!r})")
    if present:
        sys.exit(7)
"""


def _validate_frontmatter_via_schema(fm: Dict) -> Optional[str]:
    """Return a per-field refusal report if *fm* is invalid, else None.

    Refuses only on present-but-invalid fields (missing required fields are
    tolerated). Best-effort: prefers cli/.venv's python, falls back to the
    current interpreter when it has pydantic, and skips if neither is found.
    """
    venv = _CLI_SRC.parent / ".venv" / "bin" / "python"
    if venv.exists():
        py: str = str(venv)
    else:
        try:
            import pydantic  # noqa: F401
        except ImportError:
            return None
        py = sys.executable

    env = dict(os.environ)
    env["PYTHONPATH"] = os.pathsep.join(filter(None, [str(_CLI_SRC), env.get("PYTHONPATH", "")]))
    try:
        proc = subprocess.run(
            [py, "-c", _VALIDATE_SNIPPET],
            input=json.dumps(fm, default=str),
            capture_output=True,
            text=True,
            env=env,
        )
    except OSError:
        return None
    if proc.returncode == 7:
        return proc.stdout.rstrip("\n")
    return None


_WAVE_BANDS = ("low", "medium", "high")


def _wave_band(value: object, fallback: str) -> str:
    """The difficulty band a plan value names, else the fallback, else ''.

    Only the three legal bands survive: any other spelling (a typo, an int,
    True) reads as no band at all rather than leaking into `bands` output.
    """
    band = str(value or "").strip().lower()
    if band in _WAVE_BANDS:
        return band
    return fallback if fallback in _WAVE_BANDS else ""


def load_plan_strategy(
    plan_input: str,
) -> Optional[ExecutionStrategy]:
    """Resolve *plan_input* (a single-doc plan file) to an ExecutionStrategy.

    Returns ``None`` (and emits a diagnostic to stderr) on a missing/unreadable
    plan file or a missing/malformed ``## Execution Strategy`` section.

    Args:
        plan_input: Path string to a plan ``.md`` file.

    Returns:
        Parsed :class:`ExecutionStrategy` or ``None`` on any failure.
    """
    plan_path = Path(plan_input)

    try:
        from fno.plan._doc import load_plan
    except ImportError as exc:
        print(f"Warning: fno.plan._doc not importable: {exc}", file=sys.stderr)
        return None

    try:
        doc = load_plan(plan_path)
    except OSError as exc:
        print(
            f"BLOCKED blocked_reason=plan_unreadable: {exc}",
            file=sys.stderr,
        )
        return None
    except Exception as exc:
        print(
            f"Error: malformed plan doc (Execution Strategy YAML): {exc}",
            file=sys.stderr,
        )
        return None

    # Validate the plan's frontmatter against the canonical schema at the moment
    # it starts driving execution (US2). A present-but-invalid field (a bad
    # status, size, or timestamp) refuses loudly with a per-field report - not a
    # half-run. Missing required fields are tolerated (a plan may bind its node
    # later). Best-effort defense-in-depth; the blueprint save and finalize's
    # post-stamp check also guard the schema.
    schema_report = _validate_frontmatter_via_schema(doc.frontmatter)
    if schema_report is not None:
        print(
            f"BLOCKED blocked_reason=plan_frontmatter_invalid: {plan_path}\n{schema_report}",
            file=sys.stderr,
        )
        return None

    strategy_body = doc.get_section("Execution Strategy")
    if strategy_body is None:
        print(
            f"Warning: No execution strategy section found in {plan_path}",
            file=sys.stderr,
        )
        return None

    # Delegate YAML extraction + parse + normalization to the canonical
    # parser in fno.plan.brief. Single source of truth for the
    # Execution Strategy schema; operator only converts the dict result
    # into its ExecutionStrategy/Wave dataclasses. Addresses the
    # duplicate-parsing finding from Gemini review on PR #283.
    # Aliased import: orchestrator has its own parse_execution_strategy
    # function (line 690) that operates on file paths and returns
    # ExecutionStrategy; the brief module's function operates on a YAML
    # text body and returns dict. Different signatures, same name - keep
    # the local one accessible via its bare name.
    try:
        from fno.plan.brief import (
            parse_execution_strategy as _brief_parse_strategy,
            validate_task_edges as _validate_task_edges,
            BriefParseError,
        )
    except ImportError as exc:
        print(f"Warning: fno.plan.brief not importable: {exc}", file=sys.stderr)
        return None

    try:
        raw = _brief_parse_strategy(strategy_body)
    except BriefParseError as exc:
        print(
            f"Error: malformed Execution Strategy YAML in {plan_path}: {exc}",
            file=sys.stderr,
        )
        return None

    # Declared per-task edges are validated at plan-load time, beside the
    # frontmatter schema: an unknown dependency or a cycle refuses the run
    # loudly instead of deadlocking a wave mid-flight.
    edge_errors = _validate_task_edges(raw)
    if edge_errors:
        print(
            f"BLOCKED blocked_reason=plan_task_edges_invalid: {plan_path}\n"
            + "\n".join(edge_errors),
            file=sys.stderr,
        )
        return None

    execution_mode = raw.get("execution_mode", "sequential")
    scope = raw.get("scope", "single-project")
    project_tasks: Dict[str, List[str]] = raw.get("projects", {}) or {}
    # Blueprint stamps difficulty per wave; the plan's frontmatter band is the
    # fallback so a partially-stamped strategy still reports a band. No model
    # or route lives here: the band is the axis, config maps it to a lane.
    plan_band = _wave_band(doc.frontmatter.get("difficulty"), "")
    waves: List[Wave] = []

    for wave_data in raw.get("waves", []):
        if not isinstance(wave_data, dict):
            continue
        tasks_raw = wave_data.get("tasks", [])
        if isinstance(tasks_raw, list):
            tasks = [str(t) for t in tasks_raw]
        else:
            tasks = [str(tasks_raw)]
        waves.append(
            Wave(
                number=int(wave_data.get("wave", len(waves) + 1)),
                mode=str(wave_data.get("mode", "sequential")),
                tasks=tasks,
                reason=str(wave_data.get("reason", "")),
                difficulty=_wave_band(wave_data.get("difficulty"), plan_band),
            )
        )

    if not waves:
        print(
            f"Error: No valid waves found in Execution Strategy of {plan_path}",
            file=sys.stderr,
        )
        return None

    return ExecutionStrategy(
        execution_mode=str(execution_mode),
        waves=waves,
        scope=str(scope),
        project_tasks=project_tasks,
        blocked_by={
            str(t["id"]): [str(d) for d in t.get("blocked_by", [])]
            for t in raw.get("tasks", [])
            if isinstance(t, dict) and t.get("id") and t.get("blocked_by_declared", False)
        },
        task_surfaces={
            str(t["id"]): [str(s) for s in t.get("surface", [])]
            for t in raw.get("tasks", [])
            if isinstance(t, dict) and t.get("id")
        },
    )


if __name__ == "__main__":
    import sys

    import json

    if len(sys.argv) < 2 or sys.argv[1] in ('-h', '--help'):
        print("Usage: orchestrator.py <path-to-plan.md> [--state <STATE.md>]")
        print()
        print("Commands:")
        print("  orchestrator.py <index>                  Parse and display execution strategy")
        print("  orchestrator.py <index> --next            Show next wave to execute")
        print("  orchestrator.py <index> --ready [--state <state>] [--node <id>]")
        print("                                            Print dispatchable tasks as JSON")
        print("  orchestrator.py <index> --wave-decision N [--harness codex|--provider codex]")
        print("                                           Show effective execution mode for a wave")
        print("  orchestrator.py --agent <description>     Determine agent for task")
        print()
        print("Domain Routing (all tasks use archer agent):")
        print("  --agent 'Build React component'  → archer (frontend)")
        print("  --agent 'Create API endpoint'    → archer (backend)")
        print("  --agent 'Setup Docker'           → archer (devops)")
        print("  --agent 'ETL pipeline'           → archer (data)")
        sys.exit(0 if len(sys.argv) > 1 and sys.argv[1] in ('-h', '--help') else 1)

    # Handle --emit-boundary first (standalone; the do skill's one-line boundary
    # emit for task_done / blocked after parse_task_result). Always exits 0:
    # emission is non-fatal and must never fail the caller's task or run.
    if sys.argv[1] == "--emit-boundary":
        if len(sys.argv) < 3:
            print("Error: --emit-boundary requires an event type", file=sys.stderr)
            sys.exit(0)
        b_type = sys.argv[2]
        b_opts = {"--run": "", "--node": "", "--task": "", "--outcome": "", "--data": "{}"}
        b_args = sys.argv[3:]
        b_i = 0
        while b_i < len(b_args):
            if b_args[b_i] in b_opts and b_i + 1 < len(b_args):
                b_opts[b_args[b_i]] = b_args[b_i + 1]
                b_i += 2
            else:
                b_i += 1
        try:
            b_data = json.loads(b_opts["--data"])
        except json.JSONDecodeError:
            b_data = {}
        emit_status_event(
            b_type,
            run=b_opts["--run"],
            node=b_opts["--node"],
            task=b_opts["--task"],
            outcome=b_opts["--outcome"],
            data=b_data,
        )
        # The boundary every outcome already passes through is also the one
        # place the task claim taken at dispatch gets settled (x-09d7 g3):
        # done releases it, blocked/FAILED gives it back to pending. The event
        # TYPE is gated, not just trusted: a typo'd type carrying --task must
        # not reach the give-back and release a live worker's claim. The node
        # falls back to the manifest exactly as the emit envelope does - the
        # documented per-task invocations pass --task only.
        if b_opts["--task"] and b_type in ("task_done", "blocked"):
            settle_node = b_opts["--node"] or manifest_graph_node_id()
            if settle_node:
                release_task_claim_at_boundary(
                    settle_node,
                    b_opts["--task"],
                    b_opts["--outcome"] if b_type == "task_done" else "FAILED",
                )
            else:
                # Silence here reads exactly like a settle that succeeded,
                # while the claim is still held.
                print(
                    f"orchestrator: note: no node for task {b_opts['--task']}; "
                    "task claim not settled",
                    file=sys.stderr,
                )
        sys.exit(0)

    # Handle --agent flag first (standalone command, no index needed)
    if sys.argv[1] == "--agent":
        if len(sys.argv) < 3:
            print("Error: --agent requires a task description", file=sys.stderr)
            sys.exit(1)

        description = sys.argv[2]
        # Check for optional --tags
        tags = []
        if "--tags" in sys.argv:
            tags_idx = sys.argv.index("--tags")
            if tags_idx + 1 < len(sys.argv):
                tags = sys.argv[tags_idx + 1].split(",")

        agent_type = determine_agent_type_from_description(description, tags)
        agent_info = get_agent_info(agent_type)
        print(f"Agent: {agent_type}")
        print(f"Domain: {agent_info['domain']}")
        print(f"Description: {agent_info['description']}")
        sys.exit(0)

    # Parse index file for all other commands
    index_path = sys.argv[1]
    strategy = load_plan_strategy(index_path)
    # Harness identity: an explicit --harness (or legacy --provider) wins over
    # the env sniff; the sniff stays as a surfaced fallback so an absent
    # CODEX_PLUGIN_ROOT cannot silently redirect a Codex session to Claude.
    # `provider` here is the EXPLICIT arg (None when no flag); each downstream
    # resolver re-runs resolve_invoking_harness so the explicit/env-fallback
    # source is surfaced correctly rather than collapsed to "explicit".
    provider: Optional[str] = None
    for flag in ("--harness", "--provider"):
        if flag in sys.argv:
            idx = sys.argv.index(flag)
            if idx + 1 < len(sys.argv):
                provider = sys.argv[idx + 1]
            break
    try:
        resolve_invoking_harness(provider)  # validate the explicit value up front
    except ValueError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        sys.exit(2)

    if not strategy:
        print("No execution strategy found in", index_path)
        sys.exit(1)

    # Check for state file
    completed_tasks = []
    if "--state" in sys.argv:
        state_idx = sys.argv.index("--state")
        if state_idx + 1 < len(sys.argv):
            state_path = sys.argv[state_idx + 1]
            completed_tasks = get_completed_tasks_from_state(state_path)
            # --ready prints one JSON object on stdout; the informational
            # lines stay on the human-facing verbs only.
            if "--ready" not in sys.argv:
                print(f"Completed tasks from state: {completed_tasks}")
                print()

    if "--wave-decision" in sys.argv:
        wave_idx = sys.argv.index("--wave-decision")
        if wave_idx + 1 >= len(sys.argv):
            print("Error: --wave-decision requires a wave number", file=sys.stderr)
            sys.exit(1)
        try:
            wave_number = int(sys.argv[wave_idx + 1])
        except ValueError:
            print("Error: wave number must be an integer", file=sys.stderr)
            sys.exit(1)
        wave = next((item for item in strategy.waves if item.number == wave_number), None)
        if not wave:
            print(f"Error: wave {wave_number} not found", file=sys.stderr)
            sys.exit(1)
        decision = resolve_wave_execution_mode(wave, index_path, provider, strategy.task_surfaces)
        print(json.dumps(decision, indent=2))
    elif "--ready" in sys.argv:
        # The work-stealing read the waves skill runs before each dispatch
        # round. `done` rows are cross-session completions beside this
        # session's STATE.md [x]; a failed task-row read degrades to
        # STATE.md only (the same non-fatal posture as the boundary settle).
        #
        # Partition-derived edges join the declared ones first: inside a
        # parallel wave, tasks sharing a file (or a hidden shared output
        # root) serialize in wave order and an unevaluated task waits for
        # every evaluated sibling. Declared lists are unioned, never replaced.
        apply_partition_edges(strategy, index_path)
        node_id = ""
        if "--node" in sys.argv:
            node_idx = sys.argv.index("--node")
            if node_idx + 1 < len(sys.argv):
                node_id = sys.argv[node_idx + 1]
            else:
                print("Error: --node requires a node id", file=sys.stderr)
                sys.exit(1)
        completed: List[str] = list(dict.fromkeys(completed_tasks))
        claimed: List[str] = []
        blocked: List[str] = []
        own_session_ids = _ambient_session_ids()
        if node_id:
            result = _shell_fno_result(
                ["fno", "backlog", "task", "list", node_id, "--json"],
                f"task list read for node {node_id}",
            )
            rows: List[dict] = []
            if result is not None:
                try:
                    loaded = json.loads(result.stdout.decode("utf-8", "replace"))
                    if not isinstance(loaded, dict) or not isinstance(loaded.get("tasks"), list):
                        raise ValueError("task list payload must contain a tasks list")
                    rows = [r for r in loaded["tasks"] if isinstance(r, dict)]
                except (TypeError, ValueError):
                    print(
                        f"orchestrator: note: unreadable task rows for node {node_id}; "
                        "proceeding with STATE.md only",
                        file=sys.stderr,
                    )
            for row in rows:
                status = row.get("status")
                if status == "done":
                    completed.append(str(row.get("id")))
                elif status == "in_progress":
                    task_id = str(row.get("id"))
                    claim_live, claim_holder = _task_claim_state(node_id, task_id)
                    if claim_live is False:
                        # A stale task claim is exactly the recovery path that
                        # task update --status in_progress owns. Do not let a
                        # stranded graph row suppress that recovery.
                        continue
                    if claim_live is True and (
                        claim_holder in own_session_ids
                        or str(row.get("owner") or "") in own_session_ids
                    ):
                        # A resumed session may still own the live claim while
                        # its graph row is unfinished. Re-offer it so the task
                        # transition can resume the work idempotently.
                        continue
                    # An unreadable or foreign-live claim suppresses dispatch.
                    claimed.append(task_id)
                elif status == "pending":
                    continue
                else:
                    task_id = str(row.get("id"))
                    blocked.append(task_id)
                    print(
                        f"orchestrator: note: task {node_id}/{task_id} has "
                        f"unsupported status {status!r}; suppressing dispatch",
                        file=sys.stderr,
                    )
            completed = list(dict.fromkeys(completed))
            claimed = list(dict.fromkeys(claimed))
            blocked = list(dict.fromkeys(blocked))
        busy = [*claimed, *blocked]
        ready = ready_tasks(strategy, completed, busy)
        # Tasks held back by unfinished blockers, declared or derived. Not the
        # same list as `blocked` (corrupt statuses, which the skill refuses
        # on): a held task just waits for its blocker and re-enters `ready`.
        blocked_on: Dict[str, List[str]] = {}
        for wave in strategy.waves:
            for task_id in wave.tasks:
                if task_id in completed or task_id in busy or task_id in ready:
                    continue
                outstanding = sorted(effective_blockers(strategy, task_id) - set(completed))
                if outstanding:
                    blocked_on[task_id] = outstanding
        print(json.dumps({
            "ready": ready,
            "completed": completed,
            "claimed": claimed,
            "blocked": blocked,
            "blocked_on": blocked_on,
            "bands": {
                task_id: wave.difficulty
                for wave in strategy.waves
                for task_id in wave.tasks
            },
        }))
    elif "--next" in sys.argv:
        next_wave = get_next_wave(strategy, completed_tasks)
        if next_wave:
            pending = get_pending_tasks_in_wave(next_wave, completed_tasks)
            print(f"Next wave: {next_wave.number}")
            print(f"Mode: {next_wave.mode}")
            print(f"Pending tasks: {pending}")
        else:
            print("All waves complete!")
    else:
        print_wave_summary(strategy, index_path, provider)
