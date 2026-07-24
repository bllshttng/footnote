"""Preserve-existing-frontmatter contract for skills/blueprint/scripts/mutate_doc.py.

When /blueprint re-runs (or runs for the first time) against a design doc whose
author already declared `execution_mode`, `waves`, or `kill_criteria` in
frontmatter, those values must be preserved. The script previously clobbered
them with defaults (`mixed` / `[1]` / flat dict format), which silently
regressed user-authored planning detail.

These tests pin the preserve-if-set behavior.
"""
from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest
import yaml

_REPO_ROOT = Path(__file__).resolve().parents[4]
_SCRIPT_PATH = _REPO_ROOT / "skills" / "blueprint" / "scripts" / "mutate_doc.py"


def _load_mutate_module():
    spec = importlib.util.spec_from_file_location("mutate_doc_under_test", _SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["mutate_doc_under_test"] = module
    spec.loader.exec_module(module)
    return module


_mutate_doc = _load_mutate_module()


_DOC_WITH_AUTHOR_FRONTMATTER = """---
title: example spec
sources:
  - internal/fno/plans/20260721-retro-synthesis-x304c.md
  - AGENTS.md#pitfalls-corpus-capped
status: design
execution_mode: sequential
waves:
  - 1
  - 2
  - 3
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 20
    reason: Too many iterations
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: Same test failing
---

# Example

## Overview

Body text.

## Architecture

- `path/to/thing.py`

## Failure Modes

**Boundaries**
- handle zero events

**Errors**
- surface error X

**Invariants**
- preserve invariant Y

**Concurrency**
- handle race Z

## Open Questions

- Question one?
"""


_DOC_WITHOUT_AUTHOR_FRONTMATTER = """---
title: example spec
status: design
---

# Example

## Overview

Body text.

## Architecture

- `path/to/thing.py`

## Failure Modes

**Boundaries**
- handle zero events

**Errors**
- surface error X

**Invariants**
- preserve invariant Y

**Concurrency**
- handle race Z

## Open Questions

- Question one?
"""


def _extract_frontmatter(doc_text: str) -> dict:
    assert doc_text.startswith("---\n")
    rest = doc_text[4:]
    end = rest.find("\n---")
    assert end != -1
    fm_yaml = rest[:end]
    return yaml.safe_load(fm_yaml)


def test_preserve_author_execution_mode(tmp_path: Path) -> None:
    """Author-set execution_mode (e.g. 'sequential') must survive mutation."""
    doc = tmp_path / "spec.md"
    doc.write_text(_DOC_WITH_AUTHOR_FRONTMATTER, encoding="utf-8")

    rc, proposed = _mutate_doc.mutate(doc, mode="greenfield", rewrite=False, no_emit=True)
    assert rc == 0, f"mutate failed: {proposed}"

    fm = _extract_frontmatter(proposed)
    assert fm["execution_mode"] == "sequential", (
        f"author execution_mode was clobbered: got {fm['execution_mode']!r}"
    )


def test_preserve_author_waves(tmp_path: Path) -> None:
    """Author-set waves list (e.g. [1, 2, 3]) must survive mutation."""
    doc = tmp_path / "spec.md"
    doc.write_text(_DOC_WITH_AUTHOR_FRONTMATTER, encoding="utf-8")

    rc, proposed = _mutate_doc.mutate(doc, mode="greenfield", rewrite=False, no_emit=True)
    assert rc == 0, f"mutate failed: {proposed}"

    fm = _extract_frontmatter(proposed)
    assert fm["waves"] == [1, 2, 3], (
        f"author waves were clobbered: got {fm['waves']!r}"
    )


def test_preserve_author_kill_criteria(tmp_path: Path) -> None:
    """Author-set kill_criteria with name/predicate/reason must survive mutation."""
    doc = tmp_path / "spec.md"
    doc.write_text(_DOC_WITH_AUTHOR_FRONTMATTER, encoding="utf-8")

    rc, proposed = _mutate_doc.mutate(doc, mode="greenfield", rewrite=False, no_emit=True)
    assert rc == 0, f"mutate failed: {proposed}"

    fm = _extract_frontmatter(proposed)
    kc = fm["kill_criteria"]
    assert isinstance(kc, list) and len(kc) == 2, f"author kill_criteria clobbered: {kc!r}"
    # First entry should be the named iteration_ceiling, NOT the default flat dict.
    assert kc[0].get("name") == "iteration_ceiling", (
        f"author kill_criteria replaced with defaults: {kc!r}"
    )
    assert kc[0].get("predicate") == "iteration > 20"
    assert kc[0].get("reason") == "Too many iterations"


def test_defaults_applied_when_author_omits_fields(tmp_path: Path) -> None:
    """When the author hasn't set the fields, defaults still land."""
    doc = tmp_path / "spec.md"
    doc.write_text(_DOC_WITHOUT_AUTHOR_FRONTMATTER, encoding="utf-8")

    rc, proposed = _mutate_doc.mutate(doc, mode="greenfield", rewrite=False, no_emit=True)
    assert rc == 0, f"mutate failed: {proposed}"

    fm = _extract_frontmatter(proposed)
    assert fm["execution_mode"] == "mixed", (
        f"expected default execution_mode='mixed' when author omitted; got {fm['execution_mode']!r}"
    )
    assert fm["waves"] == [1], (
        f"expected default waves=[1] when author omitted; got {fm['waves']!r}"
    )
    assert fm["kill_criteria"], "expected default kill_criteria when author omitted"


def test_preserve_holds_under_rewrite(tmp_path: Path) -> None:
    """Rewrite mode (re-running on status:ready) must also preserve author values."""
    # Same doc but bumped to status:ready so we can exercise --rewrite
    doc_ready = _DOC_WITH_AUTHOR_FRONTMATTER.replace("status: design", "status: ready")
    doc = tmp_path / "spec.md"
    doc.write_text(doc_ready, encoding="utf-8")

    rc, proposed = _mutate_doc.mutate(doc, mode="greenfield", rewrite=True, no_emit=True)
    assert rc == 0, f"mutate failed: {proposed}"

    fm = _extract_frontmatter(proposed)
    assert fm["execution_mode"] == "sequential"
    assert fm["waves"] == [1, 2, 3]
    assert fm["kill_criteria"][0].get("name") == "iteration_ceiling"


def test_empty_kill_criteria_treated_as_unset(tmp_path: Path) -> None:
    """An empty kill_criteria list (or None) should be treated as unset and get defaults."""
    doc_empty = _DOC_WITH_AUTHOR_FRONTMATTER.replace(
        """kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 20
    reason: Too many iterations
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: Same test failing""",
        "kill_criteria: []",
    )
    doc = tmp_path / "spec.md"
    doc.write_text(doc_empty, encoding="utf-8")

    rc, proposed = _mutate_doc.mutate(doc, mode="greenfield", rewrite=False, no_emit=True)
    assert rc == 0, f"mutate failed: {proposed}"

    fm = _extract_frontmatter(proposed)
    assert fm["kill_criteria"], (
        "empty kill_criteria should be replaced with defaults, not left empty"
    )


def test_preserve_author_sources(tmp_path: Path) -> None:
    """The think-stamped sources: provenance list must survive blueprint mutation.

    /blueprint transcribes sources: verbatim (no parser change - new_fm starts
    from a copy of the loaded frontmatter). This pins the carry-through so a
    future strict-schema or key-stripping change cannot silently drop it.
    """
    doc = tmp_path / "spec.md"
    doc.write_text(_DOC_WITH_AUTHOR_FRONTMATTER, encoding="utf-8")

    rc, proposed = _mutate_doc.mutate(doc, mode="greenfield", rewrite=False, no_emit=True)
    assert rc == 0, f"mutate failed: {proposed}"

    fm = _extract_frontmatter(proposed)
    assert fm["sources"] == [
        "internal/fno/plans/20260721-retro-synthesis-x304c.md",
        "AGENTS.md#pitfalls-corpus-capped",
    ], f"author sources: was clobbered: got {fm.get('sources')!r}"
