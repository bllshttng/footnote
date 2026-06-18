"""fno.plan.brief - scoped task brief generator for single-doc plans.

Public API:
    build_brief(doc, task_id, include_failure_modes, include_locked_decisions) -> BriefResult
    parse_execution_strategy(yaml_text) -> dict
    find_task(parsed, task_id) -> dict | None
    list_task_ids(parsed) -> list[str]
    extract_overview_paragraph(overview_text) -> str
    parse_locked_decisions(text) -> list[dict]
    parse_failure_modes(text) -> list[dict]
    parse_patterns(text) -> list[dict]
    parse_acceptance_criteria(text) -> list[dict]
    filter_entries(entries, mode, surface_paths) -> list[dict]
    filter_patterns_by_surface(entries, surface_paths) -> list[dict]
    filter_acs_for_task(entries, task_acceptance) -> list[dict]

Exit-code translation (at the CLI boundary):
    0 - success
    1 - plan file not found or unreadable
    2 - plan doesn't match contract (missing sections, task not found)
    3 - plan content malformed (YAML parse failure)
"""
from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml

from fno.plan._doc import PlanDoc, FrontmatterError, ParseError


# ---------------------------------------------------------------------------
# Errors
# ---------------------------------------------------------------------------


class BriefError(ValueError):
    """Raised when the plan doesn't match the brief contract.

    Maps to exit code 2 (contract violation).
    """


class BriefParseError(ValueError):
    """Raised when plan content is malformed and requires human inspection.

    Maps to exit code 3.
    """


# ---------------------------------------------------------------------------
# Execution Strategy parsing
# ---------------------------------------------------------------------------


def parse_execution_strategy(yaml_text: str) -> dict[str, Any]:
    """Parse the Execution Strategy section's YAML block.

    The section body may contain prose before/after a fenced YAML block.
    We extract the YAML block (first ```yaml ... ``` fence), or treat the
    entire text as YAML if no fence is found.

    Raises:
        BriefParseError: on YAML parse failure (maps to exit 3).
    """
    # Try to extract from a fenced block first
    fence_match = re.search(r"```ya?ml\s*\n(.*?)```", yaml_text, re.DOTALL | re.IGNORECASE)
    if fence_match:
        raw_yaml = fence_match.group(1)
    else:
        raw_yaml = yaml_text

    try:
        parsed = yaml.safe_load(raw_yaml)
    except yaml.YAMLError as exc:
        line: int | None = None
        mark = getattr(exc, "problem_mark", None)
        if mark is not None:
            line = mark.line
        raise BriefParseError(
            f"Execution Strategy YAML is malformed"
            + (f" at line {line + 1}" if line is not None else "")
            + f": {exc}"
        ) from exc

    if parsed is None:
        return {"tasks": [], "waves": []}
    if not isinstance(parsed, dict):
        raise BriefParseError(
            f"Execution Strategy must be a YAML mapping, got {type(parsed).__name__}"
        )

    # Normalize tasks list
    tasks = parsed.get("tasks", [])
    if not isinstance(tasks, list):
        raise BriefParseError("Execution Strategy 'tasks' must be a list")

    normalized_tasks = []
    for t in tasks:
        if not isinstance(t, dict):
            continue
        normalized_tasks.append({
            "id": str(t.get("id", "")),
            "title": str(t.get("title", "")),
            "surface": list(t.get("surface", [])),
            "verify": str(t.get("verify", "")),
            "acceptance": [str(a) for a in t.get("acceptance", [])],
            "notes": str(t.get("notes", "")).strip(),
        })

    return {
        "tasks": normalized_tasks,
        "waves": parsed.get("waves", []),
        "execution_mode": parsed.get("execution_mode", "sequential"),
    }


def find_task(parsed: dict[str, Any], task_id: str) -> dict[str, Any] | None:
    """Return the task dict for task_id, or None if not found."""
    for t in parsed.get("tasks", []):
        if t["id"] == task_id:
            return t
    return None


def list_task_ids(parsed: dict[str, Any]) -> list[str]:
    """Return all task ids from the parsed execution strategy."""
    return [t["id"] for t in parsed.get("tasks", [])]


# ---------------------------------------------------------------------------
# Section parsers
# ---------------------------------------------------------------------------


def extract_overview_paragraph(overview_text: str) -> str:
    """Return the first non-empty paragraph from the Overview section."""
    paragraphs = re.split(r"\n{2,}", overview_text.strip())
    for p in paragraphs:
        stripped = p.strip()
        if stripped:
            return stripped
    return ""


def parse_locked_decisions(text: str) -> list[dict[str, Any]]:
    """Parse the Locked Decisions section into structured entries.

    Handles the numbered list format:
        N. **Title.** Prose. *Why:* rationale. *How to apply:* application.

    Each entry dict has: number, title, rationale, application, tags, body.
    """
    entries = []
    # Match numbered items at the start of a line
    pattern = re.compile(
        r"^(\d+)\.\s+\*\*(.+?)\*\*\s*(.*?)(?=^\d+\.\s+\*\*|\Z)",
        re.MULTILINE | re.DOTALL,
    )
    for match in pattern.finditer(text):
        number = int(match.group(1))
        title_raw = match.group(2).rstrip(".")
        body = match.group(3).strip()

        # Extract *Why:* and *How to apply:*
        why_match = re.search(r"\*Why:\*\s*(.+?)(?=\*How to apply:\*|\Z)", body, re.DOTALL)
        apply_match = re.search(r"\*How to apply:\*\s*(.+)", body, re.DOTALL)

        rationale = why_match.group(1).strip().rstrip(".") if why_match else body
        application = apply_match.group(1).strip().rstrip(".") if apply_match else ""

        entries.append({
            "number": number,
            "title": title_raw.strip(),
            "rationale": rationale,
            "application": application,
            "tags": [],  # no explicit tags in current format
            "body": body,
        })

    return entries


def parse_failure_modes(text: str) -> list[dict[str, Any]]:
    """Parse the Failure Modes section into structured entries.

    Handles the format:
        **Category**
        - bullet text
        - bullet text

    Each entry dict has: category, bullet, tags.
    """
    entries = []
    current_category = "General"
    for line in text.splitlines():
        line = line.rstrip()
        # Category line: **Something**
        cat_match = re.match(r"^\*\*(.+?)\*\*\s*$", line)
        if cat_match:
            current_category = cat_match.group(1).strip()
            continue
        # Bullet line
        bullet_match = re.match(r"^[-*]\s+(.+)$", line)
        if bullet_match:
            entries.append({
                "category": current_category,
                "bullet": bullet_match.group(1).strip(),
                "tags": [],
            })
    return entries


def parse_patterns(text: str) -> list[dict[str, Any]]:
    """Parse the Patterns to Reuse section into structured entries.

    Handles markdown table format:
        | Pattern | Source | Why reuse |
        |---|---|---|
        | `code` | path | reason |
    """
    entries = []
    header_seen = False
    separator_seen = False

    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.split("|")[1:-1]]
        if len(cells) < 3:
            continue

        # Skip header row
        if not header_seen:
            header_seen = True
            continue
        # Skip separator row
        if not separator_seen:
            if all(re.match(r"^:?-+:?$", c) for c in cells if c):
                separator_seen = True
                continue

        pattern, source, why = cells[0], cells[1], cells[2]
        entries.append({
            "pattern": pattern.strip("`"),
            "source": source,
            "why": why,
        })

    return entries


def parse_acceptance_criteria(text: str) -> list[dict[str, Any]]:
    """Parse the Acceptance Criteria section into structured entries.

    Handles the bold-prefixed format:
        **ACN-TYPE:** text
    or
        **ACN-TYPE (tags):** text

    Each entry dict has: ac_type, code, text, tags.
    """
    entries = []
    # Match bold AC codes at start of line or paragraph
    pattern = re.compile(
        r"\*\*(AC\d+-([A-Z]+)(?:\s+[^*]*)?):\*\*\s*(.+?)(?=\*\*AC\d+|\Z)",
        re.DOTALL,
    )
    for match in pattern.finditer(text):
        full_code_part = match.group(1).strip()
        ac_type = match.group(2)
        ac_text = match.group(3).strip()

        # Extract the base code (e.g. AC2-HP)
        code_match = re.match(r"(AC\d+-[A-Z]+)", full_code_part)
        code = code_match.group(1) if code_match else full_code_part

        entries.append({
            "ac_type": ac_type,
            "code": code,
            "text": ac_text,
            "tags": [],
        })

    return entries


# ---------------------------------------------------------------------------
# Filtering
# ---------------------------------------------------------------------------

def _surface_path_matches(text: str, surface_paths: list[str]) -> bool:
    """Return True if any surface path's last 1-2 components appear in text."""
    text_lower = text.lower()
    for sp in surface_paths:
        parts = Path(sp).parts
        # Match on last 1 component
        if parts and parts[-1].lower() in text_lower:
            return True
        # Match on last 2 components (e.g. plan/brief.py)
        if len(parts) >= 2:
            last2 = "/".join(parts[-2:]).lower()
            if last2 in text_lower:
                return True
    return False


def filter_entries(
    entries: list[dict[str, Any]],
    mode: str,
    surface_paths: list[str],
) -> list[dict[str, Any]]:
    """Filter a list of structured entries by mode.

    mode:
        "all"      - return all entries unchanged
        "none"     - return empty list
        "relevant" - return entries whose body mentions any surface path,
                     OR entries that are untagged (fail-open)
    """
    if mode == "all":
        return list(entries)
    if mode == "none":
        return []

    # relevant mode: surface-match OR fail-open on untagged
    result = []
    for entry in entries:
        # Use the body/bullet/rationale text for matching
        body = entry.get("body", "") or entry.get("bullet", "") or entry.get("rationale", "")
        title = entry.get("title", "")
        combined_text = f"{title} {body}"

        if _surface_path_matches(combined_text, surface_paths):
            result.append(entry)
        else:
            # Fail-open: untagged entries are included by default
            # An "untagged" entry has no explicit tags list or empty tags
            if not entry.get("tags"):
                result.append(entry)

    return result


def filter_patterns_by_surface(
    entries: list[dict[str, Any]],
    surface_paths: list[str],
) -> list[dict[str, Any]]:
    """Return patterns whose source path is within the task's surface paths.

    Matching: source value substring-matches any of the task surface paths
    OR any surface path component matches within source.
    """
    result = []
    for entry in entries:
        source = entry.get("source", "")
        if _surface_path_matches(source, surface_paths) or any(
            source in sp or sp in source for sp in surface_paths
        ):
            result.append(entry)
    return result


def filter_acs_for_task(
    entries: list[dict[str, Any]],
    task_acceptance: list[str],
    globally_tagged_codes: set[str] | None = None,
) -> list[dict[str, Any]]:
    """Return ACs relevant to this task per the fail-open contract.

    Inclusion rules (per the brief spec):
    - AC code is in this task's `task_acceptance` list -> include (explicit tag).
    - AC code is NOT in `globally_tagged_codes` -> include (globally untagged; fail-open).
    - AC code IS in `globally_tagged_codes` but not in `task_acceptance` -> exclude
      (tagged for a different task).

    If `task_acceptance` is empty, return all entries (fail-open).

    `globally_tagged_codes` is the union of every task's `acceptance` codes
    across the whole plan. When `None` (legacy call sites), every entry NOT
    in `task_acceptance` is treated as untagged and included (maximal
    fail-open: the brief grows richer, never thinner, per spec).
    """
    if not task_acceptance:
        return list(entries)

    result: list[dict[str, Any]] = []
    for entry in entries:
        code = entry.get("code", "")
        if code in task_acceptance:
            result.append(entry)
            continue
        if globally_tagged_codes is None or code not in globally_tagged_codes:
            # Globally untagged - include (fail-open).
            result.append(entry)
    return result


# ---------------------------------------------------------------------------
# BriefResult
# ---------------------------------------------------------------------------


@dataclass
class BriefResult:
    """Structured brief for a single task."""

    project_context: str
    task_spec: dict[str, Any]
    acceptance_criteria: list[dict[str, Any]]
    locked_decisions: list[dict[str, Any]]
    failure_modes: list[dict[str, Any]]
    files: list[dict[str, Any]]
    patterns: list[dict[str, Any]]
    verify_command: str

    def to_json_dict(self) -> dict[str, Any]:
        """Return the fixed JSON schema dict."""
        return {
            "project_context": self.project_context,
            "task_spec": {
                "id": self.task_spec.get("id", ""),
                "title": self.task_spec.get("title", ""),
                "surface": self.task_spec.get("surface", []),
                "verify": self.task_spec.get("verify", ""),
                "acceptance": self.task_spec.get("acceptance", []),
                "notes": self.task_spec.get("notes", ""),
            },
            "acceptance_criteria": [
                {
                    "ac_type": ac.get("ac_type", ""),
                    "code": ac.get("code", ""),
                    "text": ac.get("text", ""),
                    "tags": ac.get("tags", []),
                }
                for ac in self.acceptance_criteria
            ],
            "locked_decisions": [
                {
                    "number": ld.get("number", 0),
                    "title": ld.get("title", ""),
                    "rationale": ld.get("rationale", ""),
                    "application": ld.get("application", ""),
                    "tags": ld.get("tags", []),
                }
                for ld in self.locked_decisions
            ],
            "failure_modes": [
                {
                    "category": fm.get("category", ""),
                    "bullet": fm.get("bullet", ""),
                    "tags": fm.get("tags", []),
                }
                for fm in self.failure_modes
            ],
            "files": [
                {
                    "path": f.get("path", ""),
                    "action": f.get("action", ""),
                    "notes": f.get("notes", ""),
                }
                for f in self.files
            ],
            "patterns": [
                {
                    "pattern": p.get("pattern", ""),
                    "source": p.get("source", ""),
                    "why": p.get("why", ""),
                }
                for p in self.patterns
            ],
            "verify_command": self.verify_command,
        }

    def to_markdown(self) -> str:
        """Render the brief as a ~500-800 word markdown document."""
        lines: list[str] = []

        lines.append("## Project Context")
        lines.append("")
        lines.append(self.project_context)
        lines.append("")

        lines.append(f"## Task {self.task_spec.get('id', '')}: {self.task_spec.get('title', '')}")
        lines.append("")
        if self.task_spec.get("notes"):
            lines.append(self.task_spec["notes"])
            lines.append("")

        if self.files:
            lines.append("### Files")
            lines.append("")
            for f in self.files:
                notes_str = f" ({f['notes']})" if f.get("notes") else ""
                lines.append(f"- `{f['path']}`{notes_str}")
            lines.append("")

        if self.acceptance_criteria:
            lines.append("### Acceptance Criteria")
            lines.append("")
            for ac in self.acceptance_criteria:
                lines.append(f"**{ac['code']}:** {ac['text']}")
                lines.append("")

        if self.locked_decisions:
            lines.append("### Locked Decisions")
            lines.append("")
            for ld in self.locked_decisions:
                lines.append(f"{ld['number']}. **{ld['title']}**")
                if ld.get("rationale"):
                    lines.append(f"   *Why:* {ld['rationale']}")
                if ld.get("application"):
                    lines.append(f"   *How to apply:* {ld['application']}")
            lines.append("")

        if self.failure_modes:
            lines.append("### Failure Modes")
            lines.append("")
            current_cat = None
            for fm in self.failure_modes:
                if fm["category"] != current_cat:
                    current_cat = fm["category"]
                    lines.append(f"**{current_cat}**")
                lines.append(f"- {fm['bullet']}")
            lines.append("")

        if self.patterns:
            lines.append("### Patterns to Reuse")
            lines.append("")
            for p in self.patterns:
                lines.append(f"- `{p['pattern']}` from `{p['source']}`: {p['why']}")
            lines.append("")

        lines.append("### Verify Command")
        lines.append("")
        lines.append(f"```")
        lines.append(self.verify_command)
        lines.append("```")
        lines.append("")

        return "\n".join(lines)


# ---------------------------------------------------------------------------
# Main brief builder
# ---------------------------------------------------------------------------


def _find_execution_strategy_section(doc: PlanDoc) -> str | None:
    """Return the Execution Strategy section body, checking for the exact key."""
    return doc.get_section("Execution Strategy")


def _find_locked_decisions_section(doc: PlanDoc) -> str:
    """Return Locked Decisions section text, trying multiple heading variants."""
    for name in ("Locked Decisions (DO NOT revisit)", "Locked Decisions"):
        text = doc.get_section(name)
        if text is not None:
            return text
    return ""


def build_brief(
    doc: PlanDoc,
    task_id: str,
    include_failure_modes: str = "relevant",
    include_locked_decisions: str = "relevant",
) -> BriefResult:
    """Build a BriefResult for the given task_id from a PlanDoc.

    Raises:
        BriefError: if the plan doesn't match the contract (exit 2).
        BriefParseError: if the Execution Strategy YAML is malformed (exit 3).
    """
    # Get Execution Strategy section
    exec_section = _find_execution_strategy_section(doc)
    if exec_section is None:
        raise BriefError(
            "Plan is missing ## Execution Strategy section. "
            "Run /blueprint to generate it."
        )

    # Parse YAML - let BriefParseError propagate (maps to exit 3)
    parsed = parse_execution_strategy(exec_section)

    # Find the requested task
    task = find_task(parsed, task_id)
    if task is None:
        valid_ids = list_task_ids(parsed)
        raise BriefError(
            f"Task {task_id!r} not found in Execution Strategy. "
            f"Valid task-ids: {valid_ids}"
        )

    surface_paths = task.get("surface", [])

    # Project context
    overview_text = doc.get_section("Overview") or ""
    project_context = extract_overview_paragraph(overview_text)

    # Acceptance criteria
    ac_text = doc.get_section("Acceptance Criteria") or ""
    all_acs = parse_acceptance_criteria(ac_text)
    task_acceptance = task.get("acceptance", [])
    # Union of every task's acceptance codes across the whole plan.
    # Lets filter_acs_for_task distinguish "tagged for another task"
    # from "globally untagged" (the second includes fail-open).
    globally_tagged_codes = {
        str(code)
        for t in parsed.get("tasks", [])
        for code in (t.get("acceptance") or [])
    }
    acceptance_criteria = filter_acs_for_task(
        all_acs, task_acceptance, globally_tagged_codes
    )

    # Locked decisions
    ld_text = _find_locked_decisions_section(doc)
    all_lds = parse_locked_decisions(ld_text)
    locked_decisions = filter_entries(all_lds, include_locked_decisions, surface_paths)

    # Failure modes
    fm_text = doc.get_section("Failure Modes") or ""
    all_fms = parse_failure_modes(fm_text)
    failure_modes = filter_entries(all_fms, include_failure_modes, surface_paths)

    # Files: derived from task surface list
    files = [
        {"path": p, "action": "modify", "notes": ""}
        for p in surface_paths
    ]

    # Patterns to reuse
    patterns_text = doc.get_section("Patterns to Reuse") or ""
    all_patterns = parse_patterns(patterns_text)
    patterns = filter_patterns_by_surface(all_patterns, surface_paths)

    return BriefResult(
        project_context=project_context,
        task_spec=task,
        acceptance_criteria=acceptance_criteria,
        locked_decisions=locked_decisions,
        failure_modes=failure_modes,
        files=files,
        patterns=patterns,
        verify_command=task.get("verify", ""),
    )
