"""Blueprint first-fills frontmatter for a research doc that carries none.

Think is a research skill now and writes findings with no frontmatter, so
blueprint owns frontmatter outright. These tests pin the three-way split:
a bare doc is first-filled, a malformed block still errors, and an existing
block is preserved.
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import yaml

REPO_ROOT = Path(__file__).parents[3]
MUTATE_SCRIPT = REPO_ROOT / "skills" / "blueprint" / "scripts" / "mutate_doc.py"

# A research findings doc with no frontmatter. Carries the sections blueprint's
# mutation needs (User Stories in the accepted shape) so the first-fill itself
# is what is under test, not a later refusal.
RESEARCH_DOC = """\
# Findings

## Overview
A researched feature.

## Architecture
Two new files.

## User Stories
**US1:** As a user, I want X so that Y.

## Failure Modes
- thing breaks under load
"""

MALFORMED_DOC = """\
---
status: [unclosed
---

# Doc
"""

EXISTING_FM_DOC = """\
---
status: design
created: 2026-01-01
type: think
feature: existing
---

# Doc

## Architecture
new files

## User Stories
**US1:** As a user, I want X.

## Failure Modes
- x
"""


def _run_mutate(path: Path, *extra: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(MUTATE_SCRIPT), str(path), *extra],
        capture_output=True, text=True, cwd=str(REPO_ROOT),
    )


def _load_frontmatter(path: Path) -> dict:
    text = path.read_text(encoding="utf-8")
    if not text.startswith("---"):
        return {}
    rest = text[4:]
    close = rest.find("\n---")
    if close == -1:
        return {}
    result = yaml.safe_load(rest[:close])
    return result if isinstance(result, dict) else {}


def test_research_doc_without_frontmatter_is_first_filled(tmp_path: Path) -> None:
    doc = tmp_path / "finds.md"
    doc.write_text(RESEARCH_DOC)

    result = _run_mutate(doc, "--mode", "greenfield", "--draft")

    assert result.returncode == 0, f"exit {result.returncode}\nstderr: {result.stderr}"
    fm = _load_frontmatter(doc)
    assert fm.get("status") == "design", f"first-fill must set status=design, got {fm.get('status')}"
    assert "created" in fm, "first-fill must set created"
    assert fm.get("type") == "blueprint", f"first-fill must set type=blueprint, got {fm.get('type')}"
    assert "sources" in fm, "first-fill must carry a sources slot (empty until cited)"


def test_malformed_frontmatter_still_exits_three(tmp_path: Path) -> None:
    doc = tmp_path / "bad.md"
    doc.write_text(MALFORMED_DOC)

    result = _run_mutate(doc)

    assert result.returncode == 3, f"malformed frontmatter must exit 3, got {result.returncode}"
    # The malformed block is surfaced, not silently overwritten with a first-fill.
    assert not doc.read_text().startswith("---\nstatus: design")


def test_existing_frontmatter_is_preserved(tmp_path: Path) -> None:
    doc = tmp_path / "ok.md"
    doc.write_text(EXISTING_FM_DOC)

    result = _run_mutate(doc, "--mode", "greenfield", "--draft")

    assert result.returncode == 0, f"exit {result.returncode}\nstderr: {result.stderr}"
    fm = _load_frontmatter(doc)
    assert str(fm.get("created")) == "2026-01-01", "author-set created must be preserved"
    assert fm.get("type") == "think", "author-set type must be preserved"
    assert fm.get("feature") == "existing", "author-set feature must be preserved"
