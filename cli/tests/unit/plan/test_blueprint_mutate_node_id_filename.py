"""US4 + US8 contract for skills/blueprint/scripts/mutate_doc.py (x-8af8).

US4: mutation of an already-node-bearing design-doc path writes in place - the
     file path is preserved (no rename) and the `-<node-id>` suffix is never
     dropped or duplicated into `…-x-8af8-x-8af8.md`.
US8: the script-direct path (mutate_doc.py + `fno backlog intake`, bypassing the
     /blueprint skill body) surfaces the collision-check reminder that step 3a
     would otherwise have run.
"""
from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

_REPO_ROOT = Path(__file__).resolve().parents[4]
_SCRIPT_PATH = _REPO_ROOT / "skills" / "blueprint" / "scripts" / "mutate_doc.py"


def _load_mutate_module():
    spec = importlib.util.spec_from_file_location("mutate_doc_nodeid", _SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["mutate_doc_nodeid"] = module
    spec.loader.exec_module(module)
    return module


_mutate = _load_mutate_module()

_DOC = """---
title: node-bearing spec
status: design
claims: x-8af8
---

# Example

## Overview

Body.

## Architecture

- `path/to/thing.py`

## Failure Modes

**Boundaries**
- b

**Errors**
- e

**Invariants**
- i

**Concurrency**
- c

## Open Questions

- q?
"""


def test_mutation_preserves_node_bearing_path(tmp_path: Path) -> None:
    """US4: an already-node-bearing filename is written in place, not renamed,
    and no sibling with a duplicated suffix appears."""
    doc = tmp_path / "2026-07-11-node-bearing-x-8af8.md"
    doc.write_text(_DOC, encoding="utf-8")

    rc, _ = _mutate.mutate(doc, mode="greenfield", rewrite=False, no_emit=False)
    assert rc == 0

    files = sorted(p.name for p in tmp_path.glob("*.md"))
    # Exactly the original path survives: no rename, no `-x-8af8-x-8af8.md`.
    assert files == ["2026-07-11-node-bearing-x-8af8.md"], files
    assert doc.exists() and "status: ready" in doc.read_text(encoding="utf-8")


def test_script_direct_path_surfaces_collision_reminder(tmp_path: Path, capsys) -> None:
    """US8: a successful `main()` (script-direct) write prints the collision-check
    reminder to stderr - the gate the skill body's step 3a would have run."""
    doc = tmp_path / "2026-07-11-node-bearing-x-8af8.md"
    doc.write_text(_DOC, encoding="utf-8")

    rc = _mutate.main([str(doc)])
    assert rc == 0
    err = capsys.readouterr().err
    assert "fno backlog collisions check" in err
    assert str(doc) in err


def test_dry_run_does_not_emit_collision_reminder(tmp_path: Path, capsys) -> None:
    """A --no-emit dry-run writes nothing, so it must not nudge about intake."""
    doc = tmp_path / "2026-07-11-node-bearing-x-8af8.md"
    doc.write_text(_DOC, encoding="utf-8")

    rc = _mutate.main([str(doc), "--no-emit"])
    assert rc == 0
    assert "collisions check" not in capsys.readouterr().err
