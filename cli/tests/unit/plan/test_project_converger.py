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


# ---------------------------------------------------------------------------
# Epic rollup + parent-repaint hop (x-6c2b wave 2)
# ---------------------------------------------------------------------------

_EPIC_PLAN = """\
---
node: x-epic
status: ready
type: epic
---

# epic plan
"""


def test_epic_doc_gets_rollup_counters(tmp_path):
    """An epic node's own doc carries the computed rollup counters."""
    epic_plan = _plan(tmp_path, "epic.md", _EPIC_PLAN)
    entries = [
        {"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": str(epic_plan), "_status": "ready"},
        {"id": "c1", "parent": "x-epic", "_status": "done"},
        {"id": "c2", "parent": "x-epic", "_status": "ready"},
    ]
    assert project_graph_nodes(entries, ["x-epic"], root=str(tmp_path)) == 1
    _, fields, _ = read_plan_file(epic_plan)
    # Frontmatter scalars round-trip as strings (still bareword on disk).
    assert fields["children_total"] == "2"
    assert fields["children_done"] == "1"
    assert fields["progress"] == "1/2"
    # Idempotent: a second pass over a converged epic rewrites nothing.
    assert project_graph_nodes(entries, ["x-epic"], root=str(tmp_path)) == 0


def test_child_transition_repaints_parent_epic(tmp_path):
    """AC2: projecting a child also repaints its parent epic's rollup (one hop up)."""
    epic_plan = _plan(tmp_path, "epic.md", _EPIC_PLAN)
    child_plan = _plan(tmp_path, "child.md")
    entries = [
        {"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": str(epic_plan), "_status": "ready"},
        {"id": "x-child", "slug": "child", "parent": "x-epic", "plan_path": str(child_plan), "_status": "done"},
    ]
    # Only the child id is passed; the epic doc must still be repainted.
    assert project_graph_nodes(entries, ["x-child"], root=str(tmp_path)) == 2
    _, fields, _ = read_plan_file(epic_plan)
    assert fields["children_done"] == "1"
    assert fields["progress"] == "1/1"


def test_leaf_doc_has_no_rollup_keys(tmp_path):
    """A non-epic leaf doc never gains rollup keys (they stay clean)."""
    plan = _plan(tmp_path, "child.md")
    # priority p0 differs from the doc's p2, so the projection rewrites the doc.
    entries = [{"id": "x-c", "slug": "c", "type": "feature", "plan_path": str(plan), "priority": "p0", "_status": "ready"}]
    assert project_graph_nodes(entries, ["x-c"], root=str(tmp_path)) == 1
    _, fields, _ = read_plan_file(plan)
    assert fields["priority"] == "p0"
    assert "children_total" not in fields
    assert "progress" not in fields


# ---------------------------------------------------------------------------
# Derived wave projection (x-6c2b wave 4, AC4)
# ---------------------------------------------------------------------------

_CHILD = """\
---
node: {nid}
status: ready
type: feature
---

# {nid}
"""


def test_wave_painted_on_children_and_epic(tmp_path):
    """AC4 end-to-end: children carry derived `wave`, the epic carries `waves`."""
    epic_doc = _plan(tmp_path, "epic.md", _EPIC_PLAN)
    docs = {}
    for nid in ("a", "b", "d"):
        docs[nid] = _plan(tmp_path, f"{nid}.md", _CHILD.format(nid=nid))
    entries = [
        {"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": str(epic_doc), "_status": "ready"},
        {"id": "x-a", "slug": "a", "type": "feature", "parent": "x-epic", "plan_path": str(docs["a"]), "blocked_by": []},
        {"id": "x-b", "slug": "b", "type": "feature", "parent": "x-epic", "plan_path": str(docs["b"]), "blocked_by": ["x-a"]},
        {"id": "x-d", "slug": "d", "type": "feature", "parent": "x-epic", "plan_path": str(docs["d"]), "blocked_by": ["x-b"]},
    ]
    # A single child mutation (x-b) repaints the whole epic family via the
    # sibling+ancestor expansion, so every stratum lands without a full sweep.
    project_graph_nodes(entries, ["x-b"], root=str(tmp_path))
    assert read_plan_file(docs["a"])[1]["wave"] == "0"
    assert read_plan_file(docs["b"])[1]["wave"] == "1"
    assert read_plan_file(docs["d"])[1]["wave"] == "2"
    assert read_plan_file(epic_doc)[1]["waves"] == "3"


def test_edge_edit_restratifies_siblings(tmp_path):
    """Projecting only the edited child still repaints its siblings' waves
    (one child's blocked_by change shifts the whole epic's strata)."""
    epic_doc = _plan(tmp_path, "epic.md", _EPIC_PLAN)
    a_doc = _plan(tmp_path, "a.md", _CHILD.format(nid="a"))
    d_doc = _plan(tmp_path, "d.md", _CHILD.format(nid="d"))
    entries = [
        {"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": str(epic_doc), "_status": "ready"},
        {"id": "x-a", "slug": "a", "type": "feature", "parent": "x-epic", "plan_path": str(a_doc), "blocked_by": []},
        {"id": "x-d", "slug": "d", "type": "feature", "parent": "x-epic", "plan_path": str(d_doc), "blocked_by": ["x-a"]},
    ]
    project_graph_nodes(entries, ["x-d"], root=str(tmp_path))
    assert read_plan_file(d_doc)[1]["wave"] == "1"
    # Drop x-d's blocker; project only x-d, but x-a must also repaint (it stays 0)
    # and x-d drops to 0 - and the epic waves shrink from 2 to 1.
    entries[2]["blocked_by"] = []
    project_graph_nodes(entries, ["x-d"], root=str(tmp_path))
    assert read_plan_file(d_doc)[1]["wave"] == "0"
    assert read_plan_file(epic_doc)[1]["waves"] == "1"


def test_orphaning_child_clears_stale_wave(tmp_path):
    """codex: after --parent null, the child's stale `wave` is cleared, not left."""
    child_doc = _plan(tmp_path, "child.md", _CHILD.format(nid="c") + "wave: 2\n")
    # Seed a child under an epic; project so it carries a wave.
    epic_doc = _plan(tmp_path, "epic.md", _EPIC_PLAN)
    entries = [
        {"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": str(epic_doc), "_status": "ready"},
        {"id": "x-c", "slug": "c", "type": "feature", "parent": "x-epic", "plan_path": str(child_doc), "blocked_by": []},
    ]
    project_graph_nodes(entries, ["x-c"], root=str(tmp_path))
    assert read_plan_file(child_doc)[1]["wave"] == "0"
    # Orphan it: no epic parent -> wave must be cleared.
    entries[1]["parent"] = None
    project_graph_nodes(entries, ["x-c"], root=str(tmp_path))
    assert "wave" not in read_plan_file(child_doc)[1]


def test_epic_demotion_clears_stale_waves_and_rollup(tmp_path):
    """codex: demoting an epic to a feature clears its stale waves/rollup keys."""
    epic_doc = _plan(tmp_path, "epic.md", _EPIC_PLAN)
    entries = [
        {"id": "x-epic", "slug": "epic", "type": "epic", "plan_path": str(epic_doc), "_status": "ready"},
        {"id": "x-c", "slug": "c", "type": "feature", "parent": "x-epic", "plan_path": None, "blocked_by": []},
    ]
    project_graph_nodes(entries, ["x-epic"], root=str(tmp_path))
    fields = read_plan_file(epic_doc)[1]
    assert fields.get("waves") == "1" and fields.get("children_total") == "1"
    # Demote to feature -> derived epic keys clear.
    entries[0]["type"] = "feature"
    project_graph_nodes(entries, ["x-epic"], root=str(tmp_path))
    fields2 = read_plan_file(epic_doc)[1]
    assert "waves" not in fields2
    assert "children_total" not in fields2
    assert "progress" not in fields2
