"""fno.plan._doc - markdown plan-doc parser.

Parses a plan file (YAML frontmatter + markdown body) into a PlanDoc.

Section detection respects fenced code blocks: a ``## Foo`` line inside
a triple-backtick fence is treated as body content, not a heading.

Only ``## `` (exactly two hash + space) at the start of a line outside
fenced code blocks is recognised as a top-level section heading.

Usage::

    from fno.plan._doc import load_plan, FrontmatterError, ParseError

    doc = load_plan(Path("path/to/plan.md"))
    body = doc.get_section("Overview")
"""
from __future__ import annotations

from collections import OrderedDict
from pathlib import Path
from typing import Any

import yaml


class FrontmatterError(Exception):
    """Raised when the YAML frontmatter cannot be parsed.

    Attributes:
        line: 0-based line number of the offending YAML token, sourced
              from yaml.YAMLError.problem_mark.line.  May be None when
              the yaml exception carries no position info.
    """

    def __init__(self, message: str, line: int | None = None) -> None:
        super().__init__(message)
        self.line = line


class ParseError(Exception):
    """Raised on a structural markdown parse failure (not a YAML error)."""


class PlanDoc:
    """Parsed representation of a single-doc plan file.

    Attributes:
        frontmatter: Parsed YAML frontmatter as a plain dict.
        sections: OrderedDict mapping section heading text (without the
                  leading ``## ``) to trimmed body content.  Insertion
                  order matches document order.
    """

    def __init__(
        self,
        frontmatter: dict[str, Any],
        sections: OrderedDict[str, str],
    ) -> None:
        self.frontmatter = frontmatter
        self.sections = sections

    def get_section(self, name: str) -> str | None:
        """Return the body of *name* section, or None if absent."""
        return self.sections.get(name)

    def has_section(self, name: str) -> bool:
        """Return True if *name* is a top-level section in the document."""
        return name in self.sections


# ---------------------------------------------------------------------------
# Internal parsing helpers
# ---------------------------------------------------------------------------

_FENCE_CHARS = frozenset({"```", "~~~"})


def _split_frontmatter(text: str) -> tuple[str, str]:
    """Split *text* into (frontmatter_yaml, body_markdown).

    Expects the file to start with ``---\\n``.  Returns empty string for
    frontmatter if no fence is found.
    """
    if not text.startswith("---"):
        return "", text

    # Find the closing --- on its own line (starts after first line)
    first_newline = text.index("\n") + 1
    rest = text[first_newline:]
    close_idx = rest.find("\n---")
    if close_idx == -1:
        # No closing fence - treat entire text as body (no frontmatter)
        return "", text

    frontmatter_yaml = rest[:close_idx]
    body_start = close_idx + len("\n---")
    # Skip the newline immediately after the closing ---
    body = rest[body_start:]
    if body.startswith("\n"):
        body = body[1:]
    return frontmatter_yaml, body


def _parse_frontmatter(yaml_text: str) -> dict[str, Any]:
    """Parse *yaml_text* and return the result as a dict.

    Raises:
        FrontmatterError: if yaml parsing fails, with .line set from the
            yaml exception's problem_mark when available.
    """
    try:
        result = yaml.safe_load(yaml_text)
    except yaml.YAMLError as exc:
        line: int | None = None
        mark = getattr(exc, "problem_mark", None)
        if mark is not None:
            line = mark.line
        raise FrontmatterError(str(exc), line=line) from exc

    if result is None:
        return {}
    if not isinstance(result, dict):
        raise FrontmatterError(
            f"Frontmatter must be a YAML mapping, got {type(result).__name__}",
            line=0,
        )
    return dict(result)


def _extract_sections(body: str) -> OrderedDict[str, str]:
    """Parse *body* and return an OrderedDict of section heading -> body text.

    Only ``## `` headings at the start of a line, outside fenced code
    blocks, are treated as section boundaries.  Deeper headings (###, ####,
    etc.) and headings inside fences are captured as part of the enclosing
    section's body.
    """
    sections: OrderedDict[str, str] = OrderedDict()
    current_heading: str | None = None
    current_lines: list[str] = []
    in_fence: bool = False
    fence_marker: str = ""

    def _flush(heading: str | None, lines: list[str]) -> None:
        if heading is not None:
            trimmed = "\n".join(lines).strip()
            sections[heading] = trimmed

    for raw_line in body.splitlines():
        # Detect fence open/close.  A fence starts when a line begins with
        # ``` or ~~~ (optionally followed by a language specifier).
        stripped = raw_line.strip()
        if not in_fence:
            # Check if this line opens a fence
            if stripped.startswith("```") or stripped.startswith("~~~"):
                in_fence = True
                fence_marker = stripped[:3]
                # This line is body content, not a heading
                current_lines.append(raw_line)
                continue

            # Outside a fence: check for ## heading
            if raw_line.startswith("## "):
                _flush(current_heading, current_lines)
                current_heading = raw_line[3:].rstrip()
                current_lines = []
                continue

            current_lines.append(raw_line)
        else:
            # Inside a fence: accumulate as body
            current_lines.append(raw_line)
            # Check if this line closes the fence (same marker, possibly with
            # trailing whitespace but nothing else meaningful after it)
            if stripped == fence_marker or stripped.startswith(fence_marker) and stripped.strip(
                fence_marker[0]
            ) == "":
                in_fence = False
                fence_marker = ""

    # Flush last section
    _flush(current_heading, current_lines)
    return sections


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def load_plan(path: Path) -> PlanDoc:
    """Parse a plan file at *path* and return a PlanDoc.

    Args:
        path: Path to the markdown plan document.

    Returns:
        PlanDoc with parsed frontmatter and sections.

    Raises:
        FrontmatterError: when the YAML frontmatter is malformed.  The
            exception's ``.line`` attribute carries the 0-based line number
            of the offending token.
        ParseError: on a structural markdown parse failure.
        OSError: propagated unchanged if the file cannot be read.
    """
    text = path.read_text(encoding="utf-8")
    frontmatter_yaml, body = _split_frontmatter(text)
    frontmatter = _parse_frontmatter(frontmatter_yaml)
    sections = _extract_sections(body)
    return PlanDoc(frontmatter=frontmatter, sections=sections)
