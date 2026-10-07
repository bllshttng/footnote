"""The native update verb's plan-doc repaint, pinned at its real surface.

The goldens replay compares graph rows and never the linked plan doc, so the
wave-6 retarget that deleted the python-verb repaint tests left the doc side
of the mirror with no coverage. The python `fno backlog update` verb is
retired, so these tests drive the fno-agents binary directly over a seeded
temp graph and assert the doc's frontmatter converges to the graph after the
mutation (and the graph rows where the deleted tests did). Skips when the
checkout has no dev build.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path

import pytest
import yaml

from fno.rust_binary import find_dev_binary
from tests.fixtures.graph_seed import seed_graph

_PLAN = """\
---
node: x-1234
status: ready
priority: p2
type: feature
size: S
---

# plan body
"""

_PLAN_WITH_PARENT = """\
---
node: x-1234
status: ready
priority: p2
type: feature
size: S
parent: x-epic
parent_slug: the-epic
---

# plan body
"""


def _strify(v):
    """Scalars read back as the raw strings the line-based writer emits."""
    if isinstance(v, bool):
        return str(v)
    if isinstance(v, (int, float)):
        return str(v)
    if isinstance(v, list):
        return [_strify(i) for i in v]
    return v


def _read_plan_fields(path):
    """Frontmatter of the plan doc, scalars stringified."""
    text = Path(path).read_text(encoding="utf-8")
    m = re.match(r"^---\n(.*?)\n---(?:\n|$)", text, re.DOTALL)
    fields = yaml.safe_load(m.group(1)) if m else {}
    return {k: _strify(v) for k, v in (fields or {}).items()}


def _seed(graph: Path, entries: list[dict]) -> None:
    seed_graph(graph, json.dumps({"entries": entries}, indent=2) + "\n")


def _plan(tmp_path: Path, text: str = _PLAN) -> Path:
    p = tmp_path / "plan.md"
    p.write_text(text, encoding="utf-8")
    return p


def _node(plan: Path, **over) -> dict:
    base = {
        "id": "x-1234",
        "slug": "the-node",
        "title": "The node",
        "status": "ready",
        "domain": "code",
        "project": "fno",
        "priority": "p2",
        "type": "feature",
        "size": "S",
        "plan_path": str(plan),
    }
    base.update(over)
    return base


def _epic() -> dict:
    return {
        "id": "x-epic", "slug": "the-epic", "title": "Epic", "status": "ready",
        "domain": "code", "project": "fno", "type": "epic",
    }


def _update(tmp_path: Path, *argv: str):
    """One subprocess round-trip with the dev binary; skips without one."""
    binary = find_dev_binary()
    if binary is None:
        pytest.skip("no dev fno-agents build under crates/fno-agents/target")
    return subprocess.run(
        [str(binary), "backlog", "update", *argv],
        capture_output=True, text=True, timeout=60,
        env={
            "PATH": os.environ["PATH"],
            "HOME": str(tmp_path),
            "FNO_STATE_DIR": str(tmp_path),
        },
        cwd=str(tmp_path),
    )


def _rows(graph: Path) -> list[dict]:
    from fno.graph.store import read_graph_strict

    return read_graph_strict(graph)


def test_priority_repaints_doc(tmp_path):
    """`--priority p0` repaints the linked doc."""
    graph = tmp_path / "graph.json"
    plan = _plan(tmp_path)
    _seed(graph, [_node(plan)])

    res = _update(tmp_path, "x-1234", "--priority", "p0", "--blocks-everything")
    assert res.returncode == 0, res.stderr

    fields = _read_plan_fields(plan)
    assert fields["priority"] == "p0"
    assert str(fields["blocks_everything"]).lower() == "true"


def test_update_without_type_leaves_doc_type_alone(tmp_path):
    """A non-type update never drags the graph's stale `type` onto the doc."""
    graph = tmp_path / "graph.json"
    plan = _plan(tmp_path, _PLAN.replace("type: feature", "type: bug"))
    _seed(graph, [_node(plan)])  # graph still says feature

    res = _update(tmp_path, "x-1234", "--priority", "p0", "--blocks-everything")
    assert res.returncode == 0, res.stderr

    fields = _read_plan_fields(plan)
    assert fields["type"] == "bug"  # the doc's own value stands
    assert fields["priority"] == "p0"


def test_parent_null_clears_doc_mirror(tmp_path):
    """De-orphaning (`--parent null`) clears stale parent/parent_slug."""
    graph = tmp_path / "graph.json"
    plan = _plan(tmp_path, _PLAN_WITH_PARENT)
    _seed(graph, [_epic(), _node(plan, parent="x-epic")])

    res = _update(tmp_path, "x-1234", "--parent", "null")
    assert res.returncode == 0, res.stderr

    row = next(r for r in _rows(graph) if r["id"] == "x-1234")
    assert row["parent"] is None
    fields = _read_plan_fields(plan)
    assert "parent" not in fields
    assert "parent_slug" not in fields


def test_size_and_parent_repaint(tmp_path):
    """`--size`/`--parent` flow through the verb into the doc."""
    graph = tmp_path / "graph.json"
    plan = _plan(tmp_path)
    _seed(graph, [_epic(), _node(plan)])

    res = _update(tmp_path, "x-1234", "--size", "L", "--parent", "x-epic")
    assert res.returncode == 0, res.stderr

    fields = _read_plan_fields(plan)
    assert fields["size"] == "L"
    assert fields["parent"] == "x-epic"
    assert fields["parent_slug"] == "the-epic"


def test_missing_plan_file_never_fails_verb(tmp_path):
    """A node whose plan_path points at a deleted file: the verb exits 0."""
    graph = tmp_path / "graph.json"
    _seed(graph, [_node(tmp_path / "deleted.md")])  # never created

    res = _update(tmp_path, "x-1234", "--priority", "p0", "--blocks-everything")
    assert res.returncode == 0, res.stderr

    row = next(r for r in _rows(graph) if r["id"] == "x-1234")
    assert row["priority"] == "p0"  # graph still committed the change


def test_untag_removes_tag(tmp_path):
    """`--untag` removes a tag from graph and doc; absent tag is a no-op."""
    graph = tmp_path / "graph.json"
    plan = _plan(tmp_path, _PLAN.replace("size: S", "size: S\ntags: [mux, ui]"))
    _seed(graph, [_node(plan, tags=["mux", "ui"])])

    res = _update(tmp_path, "x-1234", "--untag", "mux", "--untag", "gone")
    assert res.returncode == 0, res.stderr

    row = next(r for r in _rows(graph) if r["id"] == "x-1234")
    assert row["tags"] == ["ui"]
    assert _read_plan_fields(plan)["tags"] == ["ui"]


def test_malformed_tag_refused_node_unchanged(tmp_path):
    """A malformed tag exits non-zero and leaves the node unchanged."""
    graph = tmp_path / "graph.json"
    plan = _plan(tmp_path)
    _seed(graph, [_node(plan)])

    res = _update(tmp_path, "x-1234", "--tag", "Mux UX!")
    assert res.returncode != 0
    assert "lowercase-kebab" in res.stderr

    row = next(r for r in _rows(graph) if r["id"] == "x-1234")
    assert row.get("tags", []) == []  # unchanged
