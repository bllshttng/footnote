"""Unit tests for the graph-driven converger (x-5d84).

Covers `fno.plan._project.project_graph_nodes`: walk a set of node ids, resolve
each node's linked plan, inject the parent slug, and project the mirror fields.
This is the shared primitive both the instrumented verbs and `fno plan sync`
call. Best-effort per node; returns a count of docs rewritten.
"""
from __future__ import annotations

from pathlib import Path

from fno.plan._project import project_graph_nodes
from fno.plan._stamp import read_plan_file

_PLAN = """\
---
node: x-child
status: ready
priority: p2
type: feature
---

# child plan
"""


def _plan(tmp_path: Path, name: str, text: str = _PLAN) -> Path:
    p = tmp_path / name
    p.write_text(text, encoding="utf-8")
    return p


def test_projects_and_injects_parent_slug(tmp_path):
    """AC3-HP: converger resolves parent -> slug from entries and injects it."""
    plan = _plan(tmp_path, "child.md")
    entries = [
        {"id": "x-epic", "slug": "the-epic", "plan_path": None},
        {
            "id": "x-child",
            "slug": "the-child",
            "plan_path": str(plan),
            "priority": "p0",
            "parent": "x-epic",
            "size": "M",
            "_status": "ready",
        },
    ]

    assert project_graph_nodes(entries, ["x-child"], root=str(tmp_path)) == 1
    _, fields, _ = read_plan_file(plan)
    assert fields["priority"] == "p0"
    assert fields["parent"] == "x-epic"
    assert fields["parent_slug"] == "the-epic"
    assert fields["size"] == "M"


def test_dangling_parent_omits_slug(tmp_path):
    """AC2-ERR: a parent id that resolves to no node mirrors parent, omits slug."""
    plan = _plan(tmp_path, "child.md")
    entries = [
        {
            "id": "x-child",
            "slug": "the-child",
            "plan_path": str(plan),
            "priority": "p1",
            "parent": "x-gone",
        },
    ]

    assert project_graph_nodes(entries, ["x-child"], root=str(tmp_path)) == 1
    _, fields, _ = read_plan_file(plan)
    assert fields["parent"] == "x-gone"
    assert "parent_slug" not in fields


def test_no_plan_path_skipped(tmp_path):
    """AC1-EDGE: a node without a plan_path is skipped, no file created."""
    entries = [{"id": "x-a", "slug": "a", "plan_path": None, "priority": "p0"}]
    assert project_graph_nodes(entries, ["x-a"], root=str(tmp_path)) == 0
    assert list(tmp_path.iterdir()) == []


def test_missing_file_never_raises_isolates_per_node(tmp_path):
    """AC1-ERR: a node whose plan file is gone is skipped; siblings still project."""
    good = _plan(tmp_path, "good.md")
    entries = [
        {"id": "x-gone", "slug": "g", "plan_path": str(tmp_path / "missing.md"), "priority": "p0"},
        {"id": "x-good", "slug": "g2", "plan_path": str(good), "priority": "p0"},
    ]

    assert project_graph_nodes(entries, ["x-gone", "x-good"], root=str(tmp_path)) == 1
    _, fields, _ = read_plan_file(good)
    assert fields["priority"] == "p0"


def test_relative_plan_path_absolutized_against_root(tmp_path):
    """A relative plan_path is resolved against the passed root."""
    plan = _plan(tmp_path, "rel.md")
    entries = [{"id": "x-r", "slug": "r", "plan_path": "rel.md", "priority": "p0"}]

    assert project_graph_nodes(entries, ["x-r"], root=str(tmp_path)) == 1
    _, fields, _ = read_plan_file(plan)
    assert fields["priority"] == "p0"


def test_empty_ids_no_op(tmp_path):
    assert project_graph_nodes([], [], root=str(tmp_path)) == 0


def test_idempotent_second_run_zero(tmp_path):
    """AC2-EDGE: a converged doc rewrites zero files on a second pass."""
    plan = _plan(tmp_path, "child.md")
    entries = [{"id": "x-c", "slug": "c", "plan_path": str(plan), "priority": "p0", "_status": "ready"}]

    assert project_graph_nodes(entries, ["x-c"], root=str(tmp_path)) == 1
    assert project_graph_nodes(entries, ["x-c"], root=str(tmp_path)) == 0
