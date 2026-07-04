#!/usr/bin/env python3
"""
Wave orchestration logic for /fno:do waves

Handles:
- Parsing execution strategy from 00-INDEX.md
- Determining wave execution order
- Tracking wave completion status
- Resume from STATE.md
- Agent routing based on task tags/keywords
"""

import sys
import re
import os
import json
from enum import Enum
from pathlib import Path
from dataclasses import dataclass, field
from typing import List, Optional, Dict
from datetime import datetime

# ---------------------------------------------------------------------------
# Single-doc plan support imports (fno.plan._locate and _doc)
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


def detect_provider() -> str:
    """Detect current provider from environment variables."""
    if os.environ.get("CODEX_PLUGIN_ROOT"):
        return "codex"
    if os.environ.get("GEMINI_PROJECT_DIR"):
        return "gemini"
    return "claude"


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
    keyword: f"archer"
    for domain, keywords in _DOMAIN_KEYWORDS.items()
    for keyword in keywords
}


@dataclass
class Wave:
    number: int
    mode: str  # 'sequential' | 'parallel'
    tasks: List[str]
    reason: str


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


def get_task_file_targets(plan_dir: str, task_id: str) -> List[str]:
    """Return normalized file targets from a task's Files section."""
    targets: List[str] = []
    for phase_file in sorted(Path(plan_dir).glob("[0-9][0-9]*.md")):
        if phase_file.name == "00-INDEX.md":
            continue
        section = _extract_task_section(phase_file, task_id)
        if not section:
            continue

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
        if targets:
            return targets
    return targets


def _shared_output_root(path: str) -> Optional[str]:
    normalized = path.strip()
    normalized = normalized.strip("`")
    for root in HIDDEN_SHARED_OUTPUT_ROOTS:
        if normalized == root[:-1] or normalized.startswith(root):
            return root.rstrip("/")
    return None


def detect_hidden_output_conflicts(plan_dir: str, task_ids: List[str]) -> Dict[str, List[str]]:
    """Detect explicit file conflicts and shared-output-root collisions for parallel tasks."""
    by_file: Dict[str, List[str]] = {}
    by_root: Dict[str, List[str]] = {}
    file_conflicts: List[str] = []
    root_conflicts: List[str] = []

    for task_id in task_ids:
        for target in get_task_file_targets(plan_dir, task_id):
            owners = by_file.setdefault(target, [])
            owners.append(task_id)
            if len(owners) == 2:
                file_conflicts.append(target)

            shared_root = _shared_output_root(target)
            if shared_root:
                root_owners = by_root.setdefault(shared_root, [])
                if task_id not in root_owners:
                    root_owners.append(task_id)
                if len(root_owners) == 2:
                    root_conflicts.append(shared_root)

    return {
        "file_conflicts": sorted(set(file_conflicts)),
        "shared_output_conflicts": sorted(set(root_conflicts)),
    }


# Providers whose stable baseline cannot spawn concurrent Task-tool subagents,
# so a conflict-free parallel wave still downgrades to sequential main-thread
# dispatch. Claude and Codex support parallel subagents; Gemini's baseline is
# sequential (skills/do/references/waves.md). This is the one provider fact the
# wave-mode resolver still needs after the static capability matrix was removed.
SEQUENTIAL_FALLBACK_PROVIDERS = frozenset({"gemini"})


def resolve_wave_execution_mode(
    wave: Wave,
    plan_dir: str,
    provider: Optional[str] = None,
) -> Dict[str, object]:
    """Resolve effective wave mode from requested mode and hidden file/shared-output conflicts."""
    resolved_provider = provider or detect_provider()
    decision: Dict[str, object] = {
        "provider": resolved_provider,
        "provider_mode": "standard",
        "provider_upgrade_reason": "",
        "requested_mode": wave.mode,
        "effective_mode": wave.mode,
        "dispatch": "main-thread",
        "reason": "Wave is sequential by plan",
        "conflicts": {"file_conflicts": [], "shared_output_conflicts": []},
    }

    if wave.mode != "parallel":
        return decision

    conflicts = detect_hidden_output_conflicts(plan_dir, wave.tasks)
    decision["conflicts"] = conflicts
    if conflicts["file_conflicts"] or conflicts["shared_output_conflicts"]:
        decision["effective_mode"] = "sequential"
        decision["reason"] = (
            "Parallel wave downgraded because tasks share explicit files or hidden shared outputs"
        )
        return decision

    if resolved_provider in SEQUENTIAL_FALLBACK_PROVIDERS:
        decision["effective_mode"] = "sequential"
        decision["provider_mode"] = "stable_fallback"
        decision["reason"] = (
            f"Parallel wave downgraded: {resolved_provider} runs sequential "
            "main-thread (no concurrent Task-tool subagents)"
        )
        return decision

    decision["dispatch"] = "native-subagents"
    decision["reason"] = "Parallel wave has no file or shared-output conflicts"
    return decision


def parse_execution_strategy(index_path: str) -> Optional[ExecutionStrategy]:
    """Parse execution strategy YAML from 00-INDEX.md without PyYAML."""
    path = Path(index_path)
    if not path.exists():
        print(f"Warning: Index file not found: {index_path}", file=sys.stderr)
        return None

    content = path.read_text()

    # Find YAML block after "## Execution Strategy"
    match = re.search(
        r'## Execution Strategy\s*```yaml\s*(.*?)\s*```',
        content,
        re.DOTALL
    )

    if not match:
        print(f"Warning: No execution strategy section found in {index_path}", file=sys.stderr)
        return None

    strategy_text = match.group(1)
    execution_mode = "sequential"
    scope = "single-project"
    waves: List[Wave] = []
    current_wave: Optional[dict] = None
    in_projects = False
    current_project_key: Optional[str] = None
    project_tasks: Dict[str, List[str]] = {}

    for raw_line in strategy_text.splitlines():
        line = raw_line.rstrip()
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if stripped.startswith("execution_mode:"):
            execution_mode = stripped.split(":", 1)[1].strip()
            continue
        if stripped.startswith("scope:"):
            scope = stripped.split(":", 1)[1].strip()
            continue
        if stripped == "projects:":
            in_projects = True
            current_project_key = None
            continue
        if in_projects and re.match(r"^[a-zA-Z_]", stripped):
            in_projects = False
            current_project_key = None
        if re.match(r"^- wave:\s*\d+", stripped):
            if current_wave:
                waves.append(Wave(**current_wave))
            current_wave = {
                "number": int(stripped.split(":", 1)[1].strip()),
                "mode": "sequential",
                "tasks": [],
                "reason": "",
            }
            continue
        if in_projects and re.match(r"^[a-zA-Z0-9_-]+:\s*$", stripped):
            current_project_key = stripped[:-1]
            project_tasks[current_project_key] = []
            continue
        if in_projects and current_project_key and stripped.startswith("tasks:"):
            project_tasks[current_project_key] = _parse_scalar(stripped.split(":", 1)[1].strip())
            continue
        if current_wave is None:
            continue
        if stripped.startswith("mode:"):
            current_wave["mode"] = stripped.split(":", 1)[1].strip()
            continue
        if stripped.startswith("tasks:"):
            current_wave["tasks"] = _parse_scalar(stripped.split(":", 1)[1].strip())
            continue
        if stripped.startswith("reason:"):
            current_wave["reason"] = stripped.split(":", 1)[1].strip().strip('"')

    if current_wave:
        waves.append(Wave(**current_wave))

    if not waves:
        print(f"Error: No valid waves found in {index_path}", file=sys.stderr)
        return None

    return ExecutionStrategy(
        execution_mode=execution_mode,
        waves=waves,
        scope=scope,
        project_tasks=project_tasks,
    )


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
    """Get the next wave to execute based on completed tasks"""
    for wave in strategy.waves:
        wave_tasks_complete = all(
            task in completed_tasks for task in wave.tasks
        )
        if not wave_tasks_complete:
            return wave
    return None  # All waves complete


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
    plan_dir: Optional[str] = None,
    provider: Optional[str] = None,
) -> None:
    """Print a summary of the execution strategy"""
    print(f"Execution mode: {strategy.execution_mode}")
    print(f"Total waves: {len(strategy.waves)}")
    print()

    for wave in strategy.waves:
        task_count = len(wave.tasks)
        print(f"Wave {wave.number} ({wave.mode}):")
        print(f"  Tasks: {', '.join(wave.tasks)}")
        print(f"  Reason: {wave.reason}")
        if wave.mode == 'parallel':
            print(f"  Parallelism: {task_count} concurrent")
            if plan_dir:
                decision = resolve_wave_execution_mode(wave, plan_dir, provider)
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


def load_plan_strategy(
    plan_input: str,
) -> Optional[ExecutionStrategy]:
    """Resolve *plan_input* to an ExecutionStrategy regardless of plan shape.

    Accepts both folder plans (directory containing ``00-INDEX.md``) and
    single-doc plans (``.md`` files).  Folder plan support preserves existing
    behavior; single-doc support is new.

    Folder plans emit a deprecation warning to stderr pointing users at
    ``fno plan migrate-folder``.

    Returns ``None`` (and emits a diagnostic to stderr) on:
    - missing or unreadable ``00-INDEX.md`` for folder plans
    - missing or malformed ``## Execution Strategy`` for single-doc plans

    Args:
        plan_input: Path string to a plan directory or ``.md`` file.

    Returns:
        Parsed :class:`ExecutionStrategy` or ``None`` on any failure.
    """
    try:
        from fno.plan._locate import PlanNotFound, locate_plan
    except ImportError as exc:
        print(f"Warning: fno.plan._locate not importable: {exc}", file=sys.stderr)
        # Fall back to legacy folder-only parsing
        path = Path(plan_input)
        if path.is_dir():
            index_path = path / "00-INDEX.md"
            return parse_execution_strategy(str(index_path))
        return parse_execution_strategy(plan_input)

    try:
        resolved = locate_plan(plan_input)
    except PlanNotFound as exc:
        print(
            f"BLOCKED blocked_reason=plan_unreadable: {exc}",
            file=sys.stderr,
        )
        return None

    if resolved.kind == "folder":
        # Deprecation warning per AC4-EDGE
        print(
            "Warning: folder plan format deprecated; run `fno plan migrate-folder` to convert",
            file=sys.stderr,
        )
        assert resolved.index_path is not None
        try:
            return parse_execution_strategy(str(resolved.index_path))
        except OSError as exc:
            print(
                f"BLOCKED blocked_reason=plan_unreadable: {exc}",
                file=sys.stderr,
            )
            return None

    # Single-doc plan
    try:
        from fno.plan._doc import load_plan
    except ImportError as exc:
        print(f"Warning: fno.plan._doc not importable: {exc}", file=sys.stderr)
        return None

    try:
        doc = load_plan(resolved.root_path)
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

    strategy_body = doc.get_section("Execution Strategy")
    if strategy_body is None:
        print(
            f"Warning: No execution strategy section found in {resolved.root_path}",
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
            BriefParseError,
        )
    except ImportError as exc:
        print(f"Warning: fno.plan.brief not importable: {exc}", file=sys.stderr)
        return None

    try:
        raw = _brief_parse_strategy(strategy_body)
    except BriefParseError as exc:
        print(
            f"Error: malformed Execution Strategy YAML in {resolved.root_path}: {exc}",
            file=sys.stderr,
        )
        return None

    execution_mode = raw.get("execution_mode", "sequential")
    scope = raw.get("scope", "single-project")
    project_tasks: Dict[str, List[str]] = raw.get("projects", {}) or {}
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
            )
        )

    if not waves:
        print(
            f"Error: No valid waves found in Execution Strategy of {resolved.root_path}",
            file=sys.stderr,
        )
        return None

    return ExecutionStrategy(
        execution_mode=str(execution_mode),
        waves=waves,
        scope=str(scope),
        project_tasks=project_tasks,
    )


if __name__ == "__main__":
    import sys

    import json

    if len(sys.argv) < 2 or sys.argv[1] in ('-h', '--help'):
        print("Usage: orchestrator.py <path-to-00-INDEX.md> [--state <STATE.md>]")
        print()
        print("Commands:")
        print("  orchestrator.py <index>                  Parse and display execution strategy")
        print("  orchestrator.py <index> --next            Show next wave to execute")
        print("  orchestrator.py <index> --state <state>   Resume from state file")
        print("  orchestrator.py <index> --wave-decision N [--provider codex]")
        print("                                           Show effective execution mode for a wave")
        print("  orchestrator.py --agent <description>     Determine agent for task")
        print()
        print("Domain Routing (all tasks use archer agent):")
        print("  --agent 'Build React component'  → archer (frontend)")
        print("  --agent 'Create API endpoint'    → archer (backend)")
        print("  --agent 'Setup Docker'           → archer (devops)")
        print("  --agent 'ETL pipeline'           → archer (data)")
        sys.exit(0 if len(sys.argv) > 1 and sys.argv[1] in ('-h', '--help') else 1)

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
    strategy = parse_execution_strategy(index_path)
    provider = detect_provider()
    if "--provider" in sys.argv:
        provider_idx = sys.argv.index("--provider")
        if provider_idx + 1 < len(sys.argv):
            provider = sys.argv[provider_idx + 1]

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
        decision = resolve_wave_execution_mode(wave, str(Path(index_path).parent), provider)
        print(json.dumps(decision, indent=2))
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
        print_wave_summary(strategy, str(Path(index_path).parent), provider)
