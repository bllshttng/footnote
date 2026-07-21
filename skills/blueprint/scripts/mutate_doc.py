"""mutate_doc.py - /blueprint single-doc mutation behavior.

Reads a design doc (YAML frontmatter + markdown body), validates it,
appends /blueprint-owned sections (Execution Strategy, optionally
File Ownership Map and Patterns to Reuse), updates frontmatter, and
writes atomically.

Usage (script mode):
    python3 skills/blueprint/scripts/mutate_doc.py <design-doc-path>
        [--mode greenfield|brownfield|auto]
        [--rewrite]
        [--no-emit]

Exit codes:
    0  success
    1  doc already in ready (or higher) status without --rewrite; or path is
       a feature description / nonexistent file (redirect to /think)
    2  required section missing (## Failure Modes) or section ownership
       violation
    3  frontmatter status missing / invalid / unreadable
"""
from __future__ import annotations

import argparse
import copy
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from collections import OrderedDict
from pathlib import Path
from typing import Any

import yaml


class _IndentDumper(yaml.Dumper):
    """Emit indented block-lists (`  - item`), not PyYAML's default indentless
    ones (`- item` flush-left). The plan-frontmatter reader `_stamp.parse_frontmatter`
    only consumes block-list children that lead with whitespace, so flush-left
    lists (kill_criteria/waves) read as malformed and skip the graph->doc
    projection. Indenting converges output on the single on-disk shape the
    reader already round-trips."""

    def increase_indent(self, flow=False, indentless=False):
        return super().increase_indent(flow, False)


# ---------------------------------------------------------------------------
# Add cli/src to sys.path so we can import fno.plan.*
# ---------------------------------------------------------------------------
_SCRIPT_DIR = Path(__file__).resolve().parent
_REPO_ROOT = _SCRIPT_DIR.parents[2]  # skills/blueprint/scripts -> madison/
_CLI_SRC = _REPO_ROOT / "cli" / "src"
if str(_CLI_SRC) not in sys.path:
    sys.path.insert(0, str(_CLI_SRC))

# E402: these imports intentionally follow the sys.path.insert above so the
# in-repo `fno.plan.*` package resolves when run as a standalone script.
from fno.plan._doc import load_plan, FrontmatterError  # noqa: E402
from fno.plan._ownership import (  # noqa: E402
    BLUEPRINT_WRITE_ALLOWLIST,
    assert_blueprint_can_write,
    OwnershipViolation,
)
from fno.plan._status import (  # noqa: E402
    validate_transition,
    coerce_status_from_yaml,
    StatusTransitionError,
)

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

# Statuses where /blueprint can write (design, or ready with --rewrite)
_BLUEPRINT_INPUT_STATUSES = frozenset({"design", "ready"})

# Statuses where the plan is past blueprint phase (cannot mutate even with --rewrite)
_PAST_BLUEPRINT_STATUSES = frozenset({"in_progress", "in_review", "reviewing", "shipping", "shipped"})

# Default kill_criteria entries. Canonical {name, predicate, reason} shape - the
# predicate engine and validate-plan.sh both read these fields; the old flat
# one-key maps ({iteration_ceiling: 20}) were invisible to both.
_DEFAULT_KILL_CRITERIA = [
    {
        "name": "iteration_ceiling",
        "predicate": "iteration > 15",
        "reason": "Too many iterations - planning likely wrong",
    },
    {
        "name": "stuck_test",
        "predicate": "same_test_failing_for >= 3",
        "reason": "Same test failing 3+ iterations - root cause unclear",
    },
]


# ---------------------------------------------------------------------------
# Path shape detection (does this look like a file path or a description?)
# ---------------------------------------------------------------------------


def _looks_like_path(arg: str) -> bool:
    """Return True if arg looks like a file path rather than a plain description."""
    if "/" in arg:
        return True
    if arg.endswith(".md"):
        return True
    if arg.startswith(("~", "./", "../", "/")):
        return True
    return False


# ---------------------------------------------------------------------------
# Path extraction from Architecture section
# ---------------------------------------------------------------------------

# Match path-like tokens: contains "/" and has extension, OR starts with "/"
#
# The backtick branch MUST exclude newlines (``[^`\n]+``, not ``[^`]+``). With
# ``[^`]+`` a fenced code block (```` ``` ````) swallows the whole block as a
# single multi-line "path"; a long no-slash diagram line then becomes one
# >NAME_MAX path component and ``Path.exists()`` raises ENAMETOOLONG during
# auto mode detection. Restricting to a single line still matches real inline
# paths like `` `cli/src/foo.py` `` and lets the bare-path branch pick up
# genuine per-line paths inside a diagram.
_PATH_RE = re.compile(
    r"`([^`\n]+)`"  # backtick-quoted (single line only)
    r"|(?<!\w)((?:[a-zA-Z0-9_.@-]+/)+[a-zA-Z0-9_.-]+)"  # bare path token with /
    r"|((?:/[a-zA-Z0-9_.-]+)+)"  # absolute paths starting with /
)


def _extract_paths_from_architecture(arch_section: str) -> list[str]:
    """Extract file path mentions from the Architecture section body.

    A token is considered a path if it:
    - contains a "/" AND has a file extension (e.g., foo/bar.py), OR
    - is an absolute path (starts with /)
    """
    paths: list[str] = []
    seen: set[str] = set()

    for match in _PATH_RE.finditer(arch_section):
        candidate = match.group(1) or match.group(2) or match.group(3)
        if not candidate:
            continue
        candidate = candidate.strip("` \t")
        # Must look like a file (has extension or is absolute)
        has_extension = bool(re.search(r"\.[a-zA-Z0-9]+$", candidate))
        is_absolute = candidate.startswith("/")
        has_slash = "/" in candidate
        if (has_slash and has_extension) or is_absolute:
            if candidate not in seen:
                seen.add(candidate)
                paths.append(candidate)
    return paths


def _probe_paths(paths: list[str], repo_root: Path) -> tuple[list[str], list[str]]:
    """Return (existing_paths, missing_paths) relative to repo_root."""
    existing = []
    missing = []
    for p in paths:
        resolved = (repo_root / p) if not Path(p).is_absolute() else Path(p)
        if resolved.exists():
            existing.append(p)
        else:
            missing.append(p)
    return existing, missing


def _detect_mode(arch_section: str | None, repo_root: Path) -> str:
    """Auto-detect greenfield or brownfield based on Architecture paths.

    Returns 'greenfield' or 'brownfield'.
    Per Locked Decision #7: >=50% existing -> brownfield; <50% -> greenfield.
    """
    if not arch_section:
        return "greenfield"
    paths = _extract_paths_from_architecture(arch_section)
    if not paths:
        return "greenfield"
    existing, _ = _probe_paths(paths, repo_root)
    ratio = len(existing) / len(paths)
    return "brownfield" if ratio >= 0.5 else "greenfield"


# ---------------------------------------------------------------------------
# Section building
# ---------------------------------------------------------------------------


_US_ID_RE = r"US\d+[a-z]*(?:\.\d+)?"


def _parse_user_stories(us_body: str) -> list[tuple[str, str]]:
    """Parse User Stories from a section body, in document order.

    Recognizes four real-world formats produced by /think or hand-edited specs:

    1. Inline bold: ``**US1:** description...`` or ``**US1 - Title.** desc...``
    2. H3 heading + colon: ``### US1: Title`` (title text on the heading line)
    3. H3 heading + em-dash: ``### US4c.1 — Title`` (compound IDs allowed)
    4. Markdown table: ``| US1 | description | ... |`` (first cell = id,
       second cell = description; header/separator rows are ignored)

    For heading style, the heading's tail-text drives the description; if the
    tail is empty (``### US1``), the first non-blank line of the following
    paragraph is used (blockquote ``> `` markers stripped).

    Returns: list of ``(id, description)`` tuples. Duplicates by ID are
    discarded (first occurrence wins), preserving document order.
    """
    # Scan both anchors against the full body, collect all candidates with
    # their position, then sort+dedupe in document order. Doing heading-then-
    # bold in two passes would make heading style always win on duplicate IDs
    # regardless of document position; collecting first preserves the actual
    # first-occurrence rule.
    candidates: list[tuple[int, str, str]] = []  # (pos, id, description)

    # H3 heading anchor.  Tolerates ``### US1: Title``, ``### US4c.1 - Title``,
    # ``### US1 - Title``, ``### US1. Title``.  The em-dash / en-dash / colon /
    # period / hyphen separator is optional; everything after it on the heading
    # line is the title.
    #
    # NOTE: every interior whitespace class MUST be horizontal-only (``[ \t]``),
    # NOT ``\s``.  ``\s`` includes ``\n``, which lets the regex cross paragraph
    # breaks: ``### US1\n\n**US2:** desc`` would otherwise match as a single
    # heading whose title captures ``**US2:** desc``, silently consuming US2.
    heading_re = re.compile(
        rf"^###[ \t]+({_US_ID_RE})[ \t]*[:\.\-–—]?[ \t]*([^\n]*)$",
        re.MULTILINE,
    )
    # Match a line that opens another User Story so the heading-empty fallback
    # below does not "steal" the next story's text as this story's description.
    # The trailing ``\|`` alternative treats any markdown table row as an
    # anchor: a story description never begins with a pipe, so an empty-title
    # heading (``### US1``) followed by a table must NOT swallow the table row
    # as its description. Without this, a same-ID table row (``### US1`` then
    # ``| US1 | real desc |``) would lose to the heading's garbage capture
    # under first-occurrence dedup.
    next_anchor_re = re.compile(rf"^(?:###\s+{_US_ID_RE}\b|\*\*{_US_ID_RE}\b|\|)")
    for m in heading_re.finditer(us_body):
        us_id = m.group(1)
        tail = m.group(2).strip()
        if not tail:
            # Heading had no title text; look at the following paragraph.
            rest = us_body[m.end():]
            for raw_line in rest.splitlines():
                line = raw_line.strip()
                if not line:
                    continue
                # Stop at the next section heading OR the next User Story
                # anchor (heading-style or bold-style). Without this guard,
                # `### US1\n\n**US2:** desc` would steal the US2 line.
                if line.startswith("##") or next_anchor_re.match(line):
                    break
                # Strip blockquote markers
                line = re.sub(r"^>\s*", "", line)
                tail = line
                break
        if not tail:
            continue
        candidates.append((m.start(), us_id, tail))

    # Inline-bold anchor.  Matches ``**US1:** desc``, ``**US1.** desc``,
    # ``**US1** desc``, and ``**US1 - Title.** desc``.
    #
    # The shape between the ID and the closing ``**`` MUST be a recognized
    # delimiter form -- not arbitrary words.  Without this, prose like
    # ``**US1 baseline constraints**`` (a plain bold reference, not a story
    # anchor) would be accepted as a US1 anchor; first-occurrence-wins dedup
    # would then drop the real ``**US1:**`` story later in the doc.
    #
    # Three accepted forms between ID and ``**``:
    #   1. ``[:\.]`` + optional trailing chars (``:``-anchored or ``.``-anchored)
    #   2. horizontal whitespace + ``-`` / ``–`` / ``—`` + horizontal whitespace
    #      + title text (the ``**US1 - Title.**`` style)
    #   3. nothing (``**US1**`` directly)
    #
    # Group(2) captures the rest of the SAME line after the closing ``**``
    # plus optional ``:`` and horizontal whitespace only.  ``\s`` would let
    # group(2) span a blank line into the next ``**USN:**`` marker and
    # silently drop that story (original codex P2 finding).
    bold_re = re.compile(
        rf"\*\*({_US_ID_RE})"
        rf"(?:[:\.][^*\n]*|[ \t]+[-–—][ \t]+[^*\n]+|)"
        rf"\*\*[: \t]*([^\n]*)$",
        re.MULTILINE,
    )
    for m in bold_re.finditer(us_body):
        us_id = m.group(1)
        desc = m.group(2).strip()
        if not desc:
            # Same-line description is empty; some specs put it on the next
            # line.  Walk forward like the heading-empty fallback, breaking
            # on the next anchor or section heading so we don't steal it.
            rest = us_body[m.end():]
            for raw_line in rest.splitlines():
                line = raw_line.strip()
                if not line:
                    continue
                if line.startswith("##") or next_anchor_re.match(line):
                    break
                line = re.sub(r"^>\s*", "", line)
                desc = line
                break
        if not desc:
            continue
        candidates.append((m.start(), us_id, desc))

    # Markdown-table anchor.  Matches a data row whose FIRST cell is a US id
    # and takes the SECOND cell as the description:
    #
    #   | ID  | Story                  | Acceptance |
    #   |-----|------------------------|------------|
    #   | US1 | As a user I can log in | ...        |
    #
    # The header row (first cell ``ID``) and the ``|---|`` separator row fail
    # the id pattern, so neither is treated as a story.  The second cell uses
    # ``[^|\n]`` so it stays inside the row, running to the next column
    # separator OR end of line: GitHub-flavored markdown lets a two-column row
    # omit the optional trailing pipe (``| US1 | desc``), and the leading pipe
    # is likewise optional.  ``.strip()`` below trims the trailing whitespace
    # the greedy cell capture leaves behind.
    table_re = re.compile(
        rf"^[ \t]*\|?[ \t]*({_US_ID_RE})[ \t]*\|[ \t]*([^|\n]*)",
        re.MULTILINE,
    )
    for m in table_re.finditer(us_body):
        us_id = m.group(1)
        desc = m.group(2).strip()
        if not desc:
            continue
        candidates.append((m.start(), us_id, desc))

    # Sort by position, then dedupe by ID with first-occurrence wins.
    candidates.sort(key=lambda x: x[0])
    seen: set[str] = set()
    result: list[tuple[str, str]] = []
    for _, us_id, desc in candidates:
        if us_id in seen:
            continue
        seen.add(us_id)
        result.append((us_id, desc))
    return result


def _build_execution_strategy(sections: OrderedDict[str, str]) -> str:
    """Build ## Execution Strategy content from User Stories."""
    us_body = sections.get("User Stories", "")
    tasks = []
    wave_num = 1

    stories = _parse_user_stories(us_body)
    if not stories:
        # No valid stories; emit a single default task with a warning
        print(
            "WARNING: no ## User Stories entries found; emitting single default task",
            file=sys.stderr,
        )
        tasks = [
            {
                "id": "1.1",
                "title": "implement feature",
                "surface": [],
                "verify": "# fill in verify command",
                "acceptance": [],
                "notes": "Default task; no User Stories found in design doc.",
            }
        ]
    else:
        for i, (us_id, description) in enumerate(stories, start=1):
            task: dict[str, Any] = {
                "id": f"{wave_num}.{i}",
                "title": description[:80],
                "surface": [],
                "verify": "# fill in verify command",
                "acceptance": [],
                "notes": f"Implement {us_id}.",
            }
            tasks.append(task)

    waves_yaml: dict[str, Any] = {
        "execution_mode": "mixed",
        "waves": [
            {
                "wave": wave_num,
                "mode": "sequential",
                "name": "Implementation",
                "tasks": [t["id"] for t in tasks],
            }
        ],
        "tasks": tasks,
    }

    yaml_block = yaml.dump(waves_yaml, Dumper=_IndentDumper, default_flow_style=False, sort_keys=False, allow_unicode=True)
    return f"```yaml\n{yaml_block}```"


def _build_file_ownership_map(arch_section: str | None, repo_root: Path) -> str:
    """Build ## File Ownership Map table for brownfield mode."""
    if not arch_section:
        return "| File | Action | Owner |\n|---|---|---|\n"

    paths = _extract_paths_from_architecture(arch_section)
    if not paths:
        return "| File | Action | Owner |\n|---|---|---|\n"

    existing, missing = _probe_paths(paths, repo_root)
    existing_set = set(existing)

    rows = ["| File | Action | Owner |", "|---|---|---|"]
    for p in paths:
        action = "modify" if p in existing_set else "create"
        rows.append(f"| `{p}` | {action} | /blueprint |")
    return "\n".join(rows)


def _build_patterns_to_reuse() -> str:
    """Build ## Patterns to Reuse section (v1: placeholder)."""
    return "*(populate during implementation)*"


# ---------------------------------------------------------------------------
# Document reconstruction helpers
# ---------------------------------------------------------------------------


def _serialize_frontmatter(fm: dict[str, Any]) -> str:
    """Serialize frontmatter dict back to YAML string (without delimiters)."""
    return yaml.dump(fm, Dumper=_IndentDumper, default_flow_style=False, sort_keys=False, allow_unicode=True).rstrip("\n")


def _reconstruct_doc(
    original_text: str,
    new_frontmatter: dict[str, Any],
    new_sections: dict[str, str],
    rewrite: bool,
) -> str:
    """Reconstruct the full doc text with updated frontmatter and appended sections.

    Args:
        original_text: Full original document text.
        new_frontmatter: Updated frontmatter dict.
        new_sections: Dict of section_name -> body_text to append/replace.
        rewrite: If True, replace existing /blueprint sections in-place.

    Returns:
        The full reconstructed document text.
    """
    # Split frontmatter from body
    body = original_text
    if original_text.startswith("---"):
        rest = original_text[4:]  # skip "---\n"
        close = rest.find("\n---")
        if close != -1:
            body = rest[close + 4:]
            if body.startswith("\n"):
                body = body[1:]

    fm_text = "---\n" + _serialize_frontmatter(new_frontmatter) + "\n---\n\n"

    if rewrite:
        # Remove existing /blueprint sections before re-appending
        body = _remove_blueprint_sections(body)

    # Find insertion point: after ## Open Questions, before ## Deferred, or EOF
    body = _append_sections_to_body(body, new_sections)

    return fm_text + body


def _remove_blueprint_sections(body: str) -> str:
    """Remove /blueprint-owned sections from body text."""
    lines = body.splitlines(keepends=True)
    result_lines = []
    skip = False
    for line in lines:
        if line.startswith("## "):
            heading = line[3:].rstrip()
            if heading in BLUEPRINT_WRITE_ALLOWLIST:
                skip = True
                continue
            else:
                skip = False
        if not skip:
            result_lines.append(line)
    return "".join(result_lines)


def _append_sections_to_body(body: str, new_sections: dict[str, str]) -> str:
    """Append new sections to body after ## Open Questions (or at EOF)."""
    lines = body.splitlines(keepends=True)

    # Find insertion point: right after ## Open Questions section, OR before
    # ## Deferred / ## Out of Scope if present, OR EOF
    insert_after_line: int = len(lines)  # default: EOF

    # Search for ## Open Questions end boundary
    in_open_questions = False
    oq_end: int | None = None
    for i, line in enumerate(lines):
        if line.startswith("## "):
            heading = line[3:].rstrip()
            if heading == "Open Questions":
                in_open_questions = True
                continue
            if in_open_questions:
                # Next ## heading after Open Questions
                oq_end = i
                break
    if oq_end is not None:
        insert_after_line = oq_end
    # If no Open Questions, check for ## Deferred / ## Out of Scope / ## References
    if oq_end is None:
        for i, line in enumerate(lines):
            if line.startswith("## "):
                heading = line[3:].rstrip()
                if heading.lower() in ("deferred", "out of scope", "references"):
                    insert_after_line = i
                    break

    # Build the new section blocks
    section_text = ""
    for name, content in new_sections.items():
        section_text += f"\n## {name}\n\n{content}\n"

    # Insert
    before = "".join(lines[:insert_after_line])
    after = "".join(lines[insert_after_line:])
    return before.rstrip("\n") + "\n" + section_text + ("\n" + after.lstrip("\n") if after.strip() else "")


# ---------------------------------------------------------------------------
# Main mutation logic
# ---------------------------------------------------------------------------


_VALIDATE_SNIPPET = r"""
import json, sys
from pydantic import ValidationError
from fno.plan.schema import PlanFrontmatter
fm = json.load(sys.stdin)
try:
    PlanFrontmatter.model_validate(fm)
except ValidationError as e:
    # Only present-but-invalid fields block; a missing required field (e.g.
    # `node`, absent on many design docs until they bind) is tolerated here.
    present = [x for x in e.errors() if x["type"] != "missing"]
    for x in present:
        loc = ".".join(str(p) for p in x["loc"]) or "<root>"
        print(f"  {loc}: {x['msg']} (got {x.get('input')!r})")
    if present:
        sys.exit(7)
"""


def _pydantic_python() -> str | None:
    """A python that can import pydantic + fno.plan.schema, or None.

    The ambient python3 that /blueprint uses lacks pydantic, so prefer the
    repo's cli/.venv (same interpreter finalize.rs resolves). Fall back to the
    current interpreter when it already has pydantic (tests under `uv run`).
    """
    venv = _REPO_ROOT / "cli" / ".venv" / "bin" / "python"
    if venv.exists():
        return str(venv)
    try:
        import pydantic  # noqa: F401

        return sys.executable
    except ImportError:
        return None


def _validate_proposed_frontmatter(new_fm: dict[str, Any]) -> str | None:
    """Return a refusal message if the proposed frontmatter is invalid, else None.

    Validates the PROPOSED frontmatter against fno.plan.schema.PlanFrontmatter,
    refusing only on present-but-invalid fields (a bad `size`, a malformed
    timestamp, an off-vocabulary status). Best-effort defense-in-depth: if no
    pydantic-capable interpreter is found, skip rather than block the save (the
    validate verb and finalize's post-stamp check also guard the schema).
    """
    py = _pydantic_python()
    if py is None:
        return None
    env = dict(os.environ)
    env["PYTHONPATH"] = os.pathsep.join(filter(None, [str(_CLI_SRC), env.get("PYTHONPATH", "")]))
    try:
        proc = subprocess.run(
            [py, "-c", _VALIDATE_SNIPPET],
            input=json.dumps(new_fm, default=str),
            capture_output=True,
            text=True,
            env=env,
        )
    except OSError:
        return None  # interpreter vanished mid-run - degrade, don't block
    if proc.returncode == 7:
        return "proposed frontmatter fails schema validation:\n" + proc.stdout.rstrip("\n")
    return None  # rc 0 = valid; any other rc (e.g. import failure) = degrade to skip


def mutate(
    doc_path: Path,
    mode: str = "auto",
    rewrite: bool = False,
    no_emit: bool = False,
    repo_root: Path | None = None,
) -> tuple[int, str]:
    """Perform the mutation and return (exit_code, result_text_or_error).

    Args:
        doc_path: Path to the design doc.
        mode: "greenfield", "brownfield", or "auto".
        rewrite: Allow re-run on status:ready.
        no_emit: Dry-run; return proposed doc without writing.
        repo_root: Repo root for path existence checks.

    Returns:
        (0, proposed_doc_text) on success.
        (N, error_message) on failure where N is the exit code.
    """
    if repo_root is None:
        repo_root = _REPO_ROOT

    # --- Arg classification: path vs description ---
    arg_str = str(doc_path)
    if not _looks_like_path(arg_str):
        msg = (
            f"No design doc found. Run `/think \"{arg_str}\"` first, then "
            f"`/blueprint <resulting-doc-path>`. Or invoke `/target` for the full chain."
        )
        return 1, msg

    # --- Resolve path ---
    resolved = doc_path.expanduser().resolve() if not doc_path.is_absolute() else doc_path
    if not resolved.exists():
        msg = (
            f"Design doc at {resolved} is missing or unreadable. "
            f"Run `/think` first to create the design doc, then pass the resulting path."
        )
        return 1, msg

    # --- Load doc ---
    try:
        plan = load_plan(resolved)
    except FrontmatterError as exc:
        return 3, f"Frontmatter parse error: {exc}"
    except OSError as exc:
        return 3, f"Cannot read {resolved}: {exc}"

    # --- Validate status ---
    raw_status = plan.frontmatter.get("status")
    if raw_status is None:
        return 3, (
            "Frontmatter 'status' is missing. Add `status: design` to the frontmatter "
            "and retry."
        )
    try:
        current_status = coerce_status_from_yaml(raw_status)
    except StatusTransitionError as exc:
        return 3, f"Frontmatter status invalid: {exc}"

    if current_status in _PAST_BLUEPRINT_STATUSES:
        return 1, (
            f"doc is in `{current_status}` status (past blueprint phase); "
            "cannot mutate even with --rewrite."
        )

    if current_status == "ready" and not rewrite:
        return 1, (
            "doc already in `ready` status; pass `rewrite` to regenerate execution sections."
        )

    # --- Validate required sections ---
    if not plan.has_section("Failure Modes"):
        return 2, (
            "design doc missing required ## Failure Modes section; run /think first."
        )

    # --- Detect mode ---
    effective_mode = mode
    if mode == "auto":
        effective_mode = _detect_mode(plan.get_section("Architecture"), repo_root)

    # --- Build sections ---
    new_sections: dict[str, str] = OrderedDict()

    # Always: Execution Strategy
    try:
        assert_blueprint_can_write("Execution Strategy")
    except OwnershipViolation as exc:
        return 2, str(exc)
    new_sections["Execution Strategy"] = _build_execution_strategy(plan.sections)

    # Brownfield only: File Ownership Map, Patterns to Reuse
    if effective_mode == "brownfield":
        for section_name in ("File Ownership Map", "Patterns to Reuse"):
            try:
                assert_blueprint_can_write(section_name)
            except OwnershipViolation as exc:
                return 2, str(exc)
        new_sections["File Ownership Map"] = _build_file_ownership_map(
            plan.get_section("Architecture"), repo_root
        )
        new_sections["Patterns to Reuse"] = _build_patterns_to_reuse()

    # --- Update frontmatter ---
    new_fm = dict(plan.frontmatter)
    # kill_criteria
    try:
        assert_blueprint_can_write("kill_criteria")
    except OwnershipViolation as exc:
        return 2, str(exc)

    # Preserve author-set values; fall back to defaults only when unset/empty.
    # The author's frontmatter encodes deliberate planning intent (e.g.
    # `execution_mode: sequential`, `waves: [1, 2, 3]`, named kill_criteria
    # with `name`/`predicate`/`reason`). Overwriting silently regresses
    # that intent.
    #
    # Each frontmatter key passes through assert_blueprint_can_write so the
    # ownership model (Locked Decision #3) covers frontmatter writes
    # uniformly, not just section writes.
    for key in ("execution_mode", "waves"):
        try:
            assert_blueprint_can_write(key)
        except OwnershipViolation as exc:
            return 2, str(exc)

    if not new_fm.get("kill_criteria"):
        new_fm["kill_criteria"] = copy.deepcopy(_DEFAULT_KILL_CRITERIA)
    if not new_fm.get("execution_mode"):
        new_fm["execution_mode"] = "mixed"
    if not new_fm.get("waves"):
        new_fm["waves"] = [1]

    # Validate and apply status transition
    if current_status == "design":
        try:
            validate_transition(current_status, "ready")
        except StatusTransitionError as exc:
            return 3, str(exc)
        new_fm["status"] = "ready"
    # If current_status == "ready" and rewrite=True, keep "ready" (identity ok for rewrite)

    # --- Validate the proposed frontmatter against the canonical schema ---
    # Refuse to write a plan whose PROPOSED frontmatter fails PlanFrontmatter,
    # naming each bad field, so /do's read weeks later never trips on it (US1).
    # Only present-but-invalid fields block (a bad `size`, a malformed
    # timestamp, an off-vocabulary status); MISSING required fields are NOT
    # enforced here - a design doc legitimately carries no `node` until it binds
    # to a backlog node (half of real design docs have none at this point).
    # Best-effort defense-in-depth (the verb and finalize also validate): if the
    # schema deps are unavailable in the ambient interpreter, skip rather than
    # block the save.
    schema_err = _validate_proposed_frontmatter(new_fm)
    if schema_err is not None:
        return 3, schema_err

    # --- Reconstruct doc ---
    original_text = resolved.read_text(encoding="utf-8")
    proposed_doc = _reconstruct_doc(
        original_text=original_text,
        new_frontmatter=new_fm,
        new_sections=new_sections,
        rewrite=rewrite,
    )

    if no_emit:
        return 0, proposed_doc

    # --- Atomic write ---
    tmp_dir = resolved.parent
    try:
        fd, tmp_name = tempfile.mkstemp(
            dir=tmp_dir, prefix=".mutate_", suffix=".tmp"
        )
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as f:
                f.write(proposed_doc)
            os.replace(tmp_name, str(resolved))
        except Exception:
            try:
                os.unlink(tmp_name)
            except OSError:
                pass
            raise
    except OSError as exc:
        return 3, f"Atomic write failed: {exc}"

    _sync_graph_status(new_fm.get("node"), resolved)
    _warn_no_file_surface(resolved)

    return 0, proposed_doc


def _warn_no_file_surface(plan_path: Path) -> None:
    """Warn when the written plan states no file surface.

    Collision detection compares plans by the files they touch, so a plan with
    no file table is invisible to it - and an empty surface is indistinguishable
    from a genuinely non-overlapping one. Deliberately does NOT auto-populate the
    map: the point is to force the surface to be stated at planning time.

    Asks the collision parser itself rather than counting table rows: a second
    heuristic would diverge from the thing it warns about, staying quiet on a
    table whose cells parse to nothing and firing on a heading it does not know.
    Without the CLI importable there is no oracle, so it says nothing.
    """
    try:
        from fno.graph.collision import has_file_surface
    except ImportError:
        return
    if has_file_surface(plan_path):
        return
    print(
        f"WARNING: {plan_path.name} states no file surface (no parseable "
        "'## File Ownership Map' or '## Files to Modify' table). Collision "
        "detection cannot compare this plan against in-flight work; fill the "
        "table in before dispatch.",
        file=sys.stderr,
    )


def _sync_graph_status(node_id: object, plan_path: Path) -> None:
    """Re-derive the node's graph ``status`` now that the doc is blueprinted.

    The graph derives `status` FROM this doc, but ``read_graph`` does not
    recompute - only a graph mutation does. Without this, a node the doc just
    moved design -> ready keeps reading `design` on every board until something
    unrelated happens to touch the graph. Re-asserting ``plan_path`` is the
    mutation that triggers the recompute, and it doubles as a self-heal for a
    path the node-id rename step moved.

    Best-effort by design: the doc is already durably written, so a missing CLI
    or a failed call warns and never fails the blueprint.
    """
    if not isinstance(node_id, str) or not node_id.strip():
        return  # unbound design doc - nothing on the graph to sync yet
    if shutil.which("fno") is None:
        return
    try:
        proc = subprocess.run(
            ["fno", "backlog", "update", node_id.strip(),
             "--plan-path", str(plan_path)],
            capture_output=True, text=True, timeout=30,
        )
        if proc.returncode != 0:
            sys.stderr.write(
                f"Warning: graph status refresh failed for {node_id}: "
                f"{proc.stderr.strip()[:200]}\n"
            )
    except Exception as exc:  # noqa: BLE001 - never fail a written doc
        sys.stderr.write(f"Warning: graph status refresh failed for {node_id}: {exc}\n")


# ---------------------------------------------------------------------------
# CLI entry point
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="/blueprint single-doc mutation: appends execution sections to a design doc.",
        prog="mutate_doc.py",
    )
    parser.add_argument("design_doc", help="Path to the design doc (markdown with frontmatter)")
    parser.add_argument(
        "--mode",
        choices=["greenfield", "brownfield", "auto"],
        default="auto",
        help="Codebase mode; default 'auto' probes Architecture paths",
    )
    parser.add_argument(
        "--rewrite",
        action="store_true",
        help="Allow re-running on a doc already at status:ready (replaces /blueprint sections)",
    )
    parser.add_argument(
        "--no-emit",
        action="store_true",
        help="Dry-run: print proposed doc to stdout without writing",
    )
    args = parser.parse_args(argv)

    doc_path = Path(args.design_doc)
    exit_code, result = mutate(
        doc_path=doc_path,
        mode=args.mode,
        rewrite=args.rewrite,
        no_emit=args.no_emit,
    )

    if exit_code != 0:
        print(result, file=sys.stderr)
    elif args.no_emit:
        print(result)
    else:
        # US8: the script-direct path (mutate_doc.py + `fno backlog intake`,
        # bypassing the full /blueprint skill body) skips step 3a's collision
        # gate. Surface it here - the one point every script-direct mutation
        # passes through - so a duplicate-file overlap is seen before intake,
        # not retroactively after ship. Surfacing, not blocking (advisory).
        print(
            "blueprint: run `fno backlog collisions check "
            f"{args.design_doc}` before `fno backlog intake` - the collision "
            "gate does not fire on the script-direct path.",
            file=sys.stderr,
        )

    return exit_code


if __name__ == "__main__":
    sys.exit(main())
