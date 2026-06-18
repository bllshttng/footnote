"""Tests for fno.plan._doc - markdown plan-doc parser.

TDD: tests written before implementation.
Task 1.1 - plan: internal/fno/plans/2026-05-18-lean-blueprint-single-doc.md
"""
from __future__ import annotations

from collections import OrderedDict
from pathlib import Path

import pytest

from fno.plan._doc import FrontmatterError, ParseError, PlanDoc, load_plan


# ---------------------------------------------------------------------------
# Fixtures / helpers
# ---------------------------------------------------------------------------

SIMPLE_PLAN = """\
---
status: design
title: My Feature
---

## Overview

This is the overview body.

## Architecture

Architecture content here.

## Failure Modes

Something might break.
"""

PLAN_WITH_CODE_FENCE = """\
---
status: design
---

## Real Section

Real body content.

```bash
## This is NOT a section heading
echo hello
```

More body content.

## Another Section

Another body.
"""

PLAN_NESTED_HEADINGS = """\
---
status: design
---

## Top Level Section

Top body.

### Sub Heading

Sub content, not a top-level section.

#### Deep Heading

Deep content.

## Second Section

Second body.
"""

PLAN_MALFORMED_YAML = """\
---
status: [invalid: yaml: here
title: blah
---

## Overview

body
"""

PLAN_NO_SECTIONS = """\
---
status: design
---

Just a paragraph, no sections.
"""

PLAN_EMPTY_SECTIONS = """\
---
status: design
---

## Section One

## Section Two

Some content.
"""

PLAN_MULTIPLE_CODE_FENCES = """\
---
status: design
---

## Setup

Before code.

```python
## Not a heading in code
def foo():
    pass
```

After code.

```yaml
## Also not a heading
key: value
```

Final text.

## Teardown

Teardown body.
"""


def write_plan(tmp_path: Path, content: str, name: str = "plan.md") -> Path:
    p = tmp_path / name
    p.write_text(content, encoding="utf-8")
    return p


# ---------------------------------------------------------------------------
# AC-HP: Basic parsing
# ---------------------------------------------------------------------------


def test_ac_hp_load_plan_parses_frontmatter(tmp_path: Path) -> None:
    """AC1-HP: frontmatter is parsed into a dict."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    assert isinstance(doc.frontmatter, dict)
    assert doc.frontmatter["status"] == "design"
    assert doc.frontmatter["title"] == "My Feature"


def test_ac_hp_load_plan_returns_plandoc(tmp_path: Path) -> None:
    """AC1-HP: load_plan returns a PlanDoc instance."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    assert isinstance(doc, PlanDoc)


def test_ac_hp_sections_are_ordered_dict(tmp_path: Path) -> None:
    """AC2-HP: sections attribute is an OrderedDict."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    assert isinstance(doc.sections, OrderedDict)


def test_ac_hp_sections_ordered_by_document_order(tmp_path: Path) -> None:
    """AC2-HP: sections preserve document order."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    section_names = list(doc.sections.keys())
    assert section_names == ["Overview", "Architecture", "Failure Modes"]


def test_ac_hp_section_body_excludes_heading_line(tmp_path: Path) -> None:
    """AC6-HP: section body does not include the heading line itself."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    body = doc.sections["Overview"]
    assert "## Overview" not in body
    assert "This is the overview body." in body


def test_ac_hp_section_body_trimmed(tmp_path: Path) -> None:
    """AC6-HP: section body is trimmed of leading/trailing blank lines."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    body = doc.sections["Architecture"]
    assert body == "Architecture content here."


def test_ac_hp_get_section_returns_body(tmp_path: Path) -> None:
    """AC1-HP: get_section returns the section body."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    assert doc.get_section("Overview") is not None
    assert "This is the overview body." in doc.get_section("Overview")  # type: ignore[arg-type]


def test_ac_hp_get_section_returns_none_for_missing(tmp_path: Path) -> None:
    """AC1-HP: get_section returns None for a section that doesn't exist."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    assert doc.get_section("Nonexistent Section") is None


def test_ac_hp_has_section_true(tmp_path: Path) -> None:
    """AC1-HP: has_section returns True for an existing section."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    assert doc.has_section("Overview") is True


def test_ac_hp_has_section_false(tmp_path: Path) -> None:
    """AC1-HP: has_section returns False for a non-existing section."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    assert doc.has_section("Missing Section") is False


# ---------------------------------------------------------------------------
# AC: Code-block fenced headings must NOT be detected as sections
# ---------------------------------------------------------------------------


def test_ac_fence_heading_not_a_section(tmp_path: Path) -> None:
    """AC1: ## inside fenced code block is NOT a section heading."""
    p = write_plan(tmp_path, PLAN_WITH_CODE_FENCE)
    doc = load_plan(p)
    assert "This is NOT a section heading" not in doc.sections
    assert "Real Section" in doc.sections
    assert "Another Section" in doc.sections


def test_ac_fence_body_contains_code_block(tmp_path: Path) -> None:
    """AC1: code fence content is part of the enclosing section's body."""
    p = write_plan(tmp_path, PLAN_WITH_CODE_FENCE)
    doc = load_plan(p)
    body = doc.sections["Real Section"]
    assert "## This is NOT a section heading" in body
    assert "echo hello" in body


def test_ac_multiple_code_fences_handled(tmp_path: Path) -> None:
    """AC1: multiple code fences in same section all handled correctly."""
    p = write_plan(tmp_path, PLAN_MULTIPLE_CODE_FENCES)
    doc = load_plan(p)
    assert list(doc.sections.keys()) == ["Setup", "Teardown"]
    body = doc.sections["Setup"]
    assert "## Not a heading in code" in body
    assert "## Also not a heading" in body


# ---------------------------------------------------------------------------
# AC: Nested headings (###) are NOT top-level sections
# ---------------------------------------------------------------------------


def test_ac_nested_headings_not_sections(tmp_path: Path) -> None:
    """AC5: ### headings are NOT top-level sections."""
    p = write_plan(tmp_path, PLAN_NESTED_HEADINGS)
    doc = load_plan(p)
    assert "Sub Heading" not in doc.sections
    assert "Deep Heading" not in doc.sections
    assert "Top Level Section" in doc.sections
    assert "Second Section" in doc.sections


def test_ac_nested_heading_content_in_parent_section(tmp_path: Path) -> None:
    """AC5: ### heading text is part of the parent ## section's body."""
    p = write_plan(tmp_path, PLAN_NESTED_HEADINGS)
    doc = load_plan(p)
    body = doc.sections["Top Level Section"]
    assert "### Sub Heading" in body
    assert "Sub content, not a top-level section." in body


# ---------------------------------------------------------------------------
# AC: FrontmatterError on malformed YAML with line number
# ---------------------------------------------------------------------------


def test_ac_err_frontmatter_error_on_malformed_yaml(tmp_path: Path) -> None:
    """AC3-ERR: FrontmatterError raised on malformed YAML frontmatter."""
    p = write_plan(tmp_path, PLAN_MALFORMED_YAML)
    with pytest.raises(FrontmatterError) as exc_info:
        load_plan(p)
    err = exc_info.value
    assert hasattr(err, "line")


def test_ac_err_frontmatter_error_has_line_number(tmp_path: Path) -> None:
    """AC3-ERR: FrontmatterError.line is populated from yaml exception."""
    p = write_plan(tmp_path, PLAN_MALFORMED_YAML)
    with pytest.raises(FrontmatterError) as exc_info:
        load_plan(p)
    assert exc_info.value.line is not None
    assert isinstance(exc_info.value.line, int)


def test_ac_err_frontmatter_is_subclass_of_exception(tmp_path: Path) -> None:
    """AC3-ERR: FrontmatterError is an Exception subclass."""
    assert issubclass(FrontmatterError, Exception)


def test_ac_err_parse_error_is_subclass_of_exception() -> None:
    """AC3-ERR: ParseError is an Exception subclass."""
    assert issubclass(ParseError, Exception)


# ---------------------------------------------------------------------------
# AC: No sections = empty OrderedDict
# ---------------------------------------------------------------------------


def test_ac_no_sections_returns_empty_dict(tmp_path: Path) -> None:
    """AC1-HP: Plan with no ## sections returns empty sections dict."""
    p = write_plan(tmp_path, PLAN_NO_SECTIONS)
    doc = load_plan(p)
    assert doc.sections == OrderedDict()


def test_ac_empty_section_body(tmp_path: Path) -> None:
    """AC6-HP: A section with no body (immediately followed by next ##) returns empty string."""
    p = write_plan(tmp_path, PLAN_EMPTY_SECTIONS)
    doc = load_plan(p)
    assert "Section One" in doc.sections
    assert doc.sections["Section One"] == ""


# ---------------------------------------------------------------------------
# AC: Large file (100KB boundary)
# ---------------------------------------------------------------------------


def test_ac_large_file_parses(tmp_path: Path) -> None:
    """AC4: Plan files up to 100KB parse without issues."""
    # Build a plan ~100KB by repeating section content
    repeated_body = "word " * 2000  # ~10KB per section
    sections = "\n\n".join(
        f"## Section {i}\n\n{repeated_body}" for i in range(10)
    )
    content = f"---\nstatus: design\n---\n\n{sections}"
    assert len(content.encode("utf-8")) > 90_000  # at least 90KB
    p = write_plan(tmp_path, content)
    doc = load_plan(p)
    assert len(doc.sections) == 10
    for i in range(10):
        assert f"Section {i}" in doc.sections


# ---------------------------------------------------------------------------
# AC: Path-based loading
# ---------------------------------------------------------------------------


def test_ac_load_plan_accepts_path_object(tmp_path: Path) -> None:
    """AC1-HP: load_plan accepts a pathlib.Path."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(Path(p))
    assert doc.frontmatter["status"] == "design"


def test_ac_section_body_multiline(tmp_path: Path) -> None:
    """AC6-HP: Section body captures multiple lines of content."""
    content = """\
---
status: design
---

## Multi Line

Line one.
Line two.
Line three.
"""
    p = write_plan(tmp_path, content)
    doc = load_plan(p)
    body = doc.sections["Multi Line"]
    assert "Line one." in body
    assert "Line two." in body
    assert "Line three." in body


def test_ac_section_body_stops_at_next_h2(tmp_path: Path) -> None:
    """AC6-HP: Section body does not bleed into next section."""
    p = write_plan(tmp_path, SIMPLE_PLAN)
    doc = load_plan(p)
    overview = doc.sections["Overview"]
    assert "Architecture content" not in overview
    assert "Something might break" not in overview
