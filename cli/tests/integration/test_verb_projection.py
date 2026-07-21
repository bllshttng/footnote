"""Integration tests for graph-mutating verbs repainting their linked docs (x-5d84).

Drives the REAL backlog verbs (update/defer/undefer/supersede) through the Typer
CliRunner against a temp graph + temp plan doc, and asserts the doc's mirror
frontmatter converges to the graph after the mutation. Covers AC1-HP (a mutating
verb repaints its touched doc) and AC1-ERR (a missing plan file never fails the
verb).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.plan._stamp import read_plan_file

runner = CliRunner()

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


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


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


def test_update_priority_repaints_doc(tmp_graph, tmp_path):
    """AC1-HP: `backlog update --priority p0` repaints the linked doc."""
    plan = _plan(tmp_path)
    _seed(tmp_graph, [_node(plan)])

    res = runner.invoke(app, ["backlog", "update", "x-1234", "--priority", "p0"])
    assert res.exit_code == 0, res.output

    _, fields, _ = read_plan_file(plan)
    assert fields["priority"] == "p0"


def test_update_size_and_parent_repaint(tmp_graph, tmp_path):
    """AC3-HP end-to-end: --size/--parent flow through the verb into the doc."""
    plan = _plan(tmp_path)
    _seed(tmp_graph, [
        {"id": "x-epic", "slug": "the-epic", "title": "Epic", "status": "ready",
         "domain": "code", "project": "fno", "type": "epic"},
        _node(plan),
    ])

    res = runner.invoke(app, ["backlog", "update", "x-1234", "--size", "L", "--parent", "x-epic"])
    assert res.exit_code == 0, res.output

    _, fields, _ = read_plan_file(plan)
    assert fields["size"] == "L"
    assert fields["parent"] == "x-epic"
    assert fields["parent_slug"] == "the-epic"


def test_supersede_repaints_both_nodes(tmp_graph, tmp_path):
    """AC1-HP: supersede repaints the old node's doc (status forward to superseded is
    a graph gate, but blocked_by/priority mirror still converges)."""
    old_plan = _plan(tmp_path)
    old = _node(old_plan, id="x-01d0", slug="old", priority="p1")
    new_plan = tmp_path / "new.md"
    new_plan.write_text(_PLAN.replace("x-1234", "x-0ec0").replace("priority: p2", "priority: p3"), encoding="utf-8")
    new = _node(new_plan, id="x-0ec0", slug="new", priority="p0")
    _seed(tmp_graph, [old, new])

    res = runner.invoke(
        app, ["backlog", "supersede", "x-0ec0", "--replaces", "x-01d0", "--reason", "dup"]
    )
    assert res.exit_code == 0, res.output
    # The new node's doc mirrors its graph priority.
    _, fields, _ = read_plan_file(new_plan)
    assert fields["priority"] == "p0"


def test_update_parent_null_clears_doc_mirror(tmp_graph, tmp_path):
    """Codex P2: de-orphaning (--parent null) clears stale parent/parent_slug."""
    plan = _plan(
        tmp_path,
        _PLAN.replace("size: S", "size: S\nparent: x-epic\nparent_slug: the-epic"),
    )
    _seed(tmp_graph, [
        {"id": "x-epic", "slug": "the-epic", "title": "Epic", "status": "ready",
         "domain": "code", "project": "fno", "type": "epic"},
        _node(plan, parent="x-epic"),
    ])

    res = runner.invoke(app, ["backlog", "update", "x-1234", "--parent", "null"])
    assert res.exit_code == 0, res.output

    _, fields, _ = read_plan_file(plan)
    assert "parent" not in fields
    assert "parent_slug" not in fields


def test_missing_plan_file_never_fails_verb(tmp_graph, tmp_path):
    """AC1-ERR: a node whose plan_path points at a deleted file - the verb exits 0."""
    gone = tmp_path / "deleted.md"  # never created
    _seed(tmp_graph, [_node(gone)])

    res = runner.invoke(app, ["backlog", "update", "x-1234", "--priority", "p0"])
    assert res.exit_code == 0, res.output
    # graph still committed the change
    entries = json.loads(tmp_graph.read_text())["entries"]
    assert entries[0]["priority"] == "p0"


def test_tag_roundtrip_reaches_doc(tmp_graph, tmp_path):
    """AC1: `update --tag mux --tag mux` stores one tag and repaints the doc."""
    plan = _plan(tmp_path)
    _seed(tmp_graph, [_node(plan)])

    res = runner.invoke(app, ["backlog", "update", "x-1234", "--tag", "mux", "--tag", "mux"])
    assert res.exit_code == 0, res.output

    entries = json.loads(tmp_graph.read_text())["entries"]
    assert entries[0]["tags"] == ["mux"]  # dedup, idempotent
    _, fields, _ = read_plan_file(plan)
    assert fields["tags"] == ["mux"]


def test_untag_removes_tag(tmp_graph, tmp_path):
    """--untag removes a tag; absent tag is a no-op, not an error."""
    plan = _plan(tmp_path)
    _seed(tmp_graph, [_node(plan, tags=["mux", "ui"])])

    res = runner.invoke(app, ["backlog", "update", "x-1234", "--untag", "mux", "--untag", "gone"])
    assert res.exit_code == 0, res.output
    entries = json.loads(tmp_graph.read_text())["entries"]
    assert entries[0]["tags"] == ["ui"]


def test_malformed_tag_refused_node_unchanged(tmp_graph, tmp_path):
    """AC1-ERR: a malformed tag exits non-zero and leaves the node unchanged."""
    plan = _plan(tmp_path)
    _seed(tmp_graph, [_node(plan)])

    res = runner.invoke(app, ["backlog", "update", "x-1234", "--tag", "Mux UX!"])
    assert res.exit_code != 0
    assert "lowercase-kebab" in res.output
    entries = json.loads(tmp_graph.read_text())["entries"]
    assert entries[0].get("tags", []) == []  # unchanged


def _epic(nid, slug, parent=None):
    return {
        "id": nid, "slug": slug, "title": slug, "status": "ready",
        "domain": "code", "project": "fno", "type": "epic", "parent": parent,
    }


def test_epic_under_mission_allowed(tmp_graph, tmp_path):
    """An epic may nest under a top-level mission (mission -> epic)."""
    _seed(tmp_graph, [_epic("x-0a01", "mission"), _epic("x-0e02", "epic")])
    res = runner.invoke(app, ["backlog", "update", "x-0e02", "--parent", "x-0a01"])
    assert res.exit_code == 0, res.output
    entries = json.loads(tmp_graph.read_text())["entries"]
    assert next(e for e in entries if e["id"] == "x-0e02")["parent"] == "x-0a01"


def test_epic_depth_cap_refused(tmp_graph, tmp_path):
    """AC3-ERR: parenting an epic under a nested epic exceeds the 2-level cap."""
    # mission M -> epic E; now try to nest epic G under E (would be 3rd level).
    _seed(tmp_graph, [
        _epic("x-0a01", "mission"),
        _epic("x-0e02", "epic", parent="x-0a01"),
        _epic("x-0c03", "gepic"),
    ])
    res = runner.invoke(app, ["backlog", "update", "x-0c03", "--parent", "x-0e02"])
    assert res.exit_code != 0
    assert "cap" in res.output.lower()
    entries = json.loads(tmp_graph.read_text())["entries"]
    assert next(e for e in entries if e["id"] == "x-0c03")["parent"] is None  # unchanged


def test_epic_owning_subtree_cannot_nest_under_mission(tmp_graph, tmp_path):
    """AC3-ERR (down-tree): reparenting an epic that already owns a child epic
    under a mission would make a 3rd epic level - refused from the other side."""
    # B is a top-level mission that owns child epic C. Nesting B under mission M
    # would create M -> B -> C (three epic levels).
    _seed(tmp_graph, [
        _epic("x-0a01", "mission-m"),
        _epic("x-0b02", "mission-b"),
        _epic("x-0c03", "child-epic", parent="x-0b02"),
    ])
    res = runner.invoke(app, ["backlog", "update", "x-0b02", "--parent", "x-0a01"])
    assert res.exit_code != 0
    assert "cap" in res.output.lower()
    entries = json.loads(tmp_graph.read_text())["entries"]
    assert next(e for e in entries if e["id"] == "x-0b02")["parent"] is None  # unchanged


def test_leaf_under_nested_epic_allowed(tmp_graph, tmp_path):
    """A leaf (feature) under an epic is always allowed - only epics are capped."""
    plan = _plan(tmp_path)
    _seed(tmp_graph, [
        _epic("x-0a01", "mission"),
        _epic("x-0e02", "epic", parent="x-0a01"),
        _node(plan),  # a feature
    ])
    res = runner.invoke(app, ["backlog", "update", "x-1234", "--parent", "x-0e02"])
    assert res.exit_code == 0, res.output


def test_add_epic_depth_cap_refused(tmp_graph, tmp_path):
    """AC3-ERR (create path): `add --type epic --parent <nested-epic>` is capped
    too, or the guard cmd_update applies would be bypassable at creation."""
    _seed(tmp_graph, [
        _epic("x-0a01", "mission"),
        _epic("x-0e02", "epic", parent="x-0a01"),
    ])
    res = runner.invoke(
        app, ["backlog", "add", "Third level epic", "--type", "epic", "--parent", "x-0e02"]
    )
    assert res.exit_code != 0
    assert "cap" in res.output.lower()
    # No new node was appended.
    entries = json.loads(tmp_graph.read_text())["entries"]
    assert len(entries) == 2


def test_add_leaf_under_epic_still_allowed(tmp_graph, tmp_path):
    """A non-epic child under an epic is unaffected by the create-path cap."""
    _seed(tmp_graph, [_epic("x-0a01", "mission"), _epic("x-0e02", "epic", parent="x-0a01")])
    res = runner.invoke(
        app, ["backlog", "add", "A feature", "--type", "feature", "--parent", "x-0e02"]
    )
    assert res.exit_code == 0, res.output


_EPIC_DOC = """\
---
node: {nid}
status: ready
type: epic
---

# {nid} epic
"""


def _epic_with_plan(tmp_path, nid, slug, parent=None):
    p = tmp_path / f"{nid}.md"
    p.write_text(_EPIC_DOC.format(nid=nid), encoding="utf-8")
    e = _epic(nid, slug, parent)
    e["plan_path"] = str(p)
    return e, p


def test_type_change_to_epic_respects_depth_cap(tmp_graph, tmp_path):
    """codex P1: promoting a feature to epic under a nested epic is capped too,
    even though --parent is not passed."""
    plan = _plan(tmp_path)
    _seed(tmp_graph, [
        _epic("x-0a01", "mission"),
        _epic("x-0e02", "epic", parent="x-0a01"),
        _node(plan, parent="x-0e02"),  # a feature under the nested epic
    ])
    res = runner.invoke(app, ["backlog", "update", "x-1234", "--type", "epic"])
    assert res.exit_code != 0
    assert "cap" in res.output.lower()
    entries = json.loads(tmp_graph.read_text())["entries"]
    assert next(e for e in entries if e["id"] == "x-1234")["type"] == "feature"  # unchanged


def test_reparent_repaints_old_epic(tmp_graph, tmp_path):
    """codex P2: moving a child off epic A onto epic B repaints A's rollup too
    (A no longer counts the moved child)."""
    epic_a, a_doc = _epic_with_plan(tmp_path, "x-0a0a", "epic-a")
    epic_b, b_doc = _epic_with_plan(tmp_path, "x-0b0b", "epic-b")
    child_plan = tmp_path / "child.md"
    child_plan.write_text(_PLAN, encoding="utf-8")
    child = _node(child_plan, id="x-0c0c", slug="child", parent="x-0a0a")
    _seed(tmp_graph, [epic_a, epic_b, child])
    # Converge the initial state so A shows its child.
    runner.invoke(app, ["backlog", "update", "x-0a0a", "--priority", "p1"])
    _, fa, _ = read_plan_file(a_doc)
    assert fa["children_total"] == "1"
    # Move the child from A to B.
    res = runner.invoke(app, ["backlog", "update", "x-0c0c", "--parent", "x-0b0b"])
    assert res.exit_code == 0, res.output
    _, fa2, _ = read_plan_file(a_doc)
    _, fb2, _ = read_plan_file(b_doc)
    assert fa2["children_total"] == "0"  # A repainted: lost the child
    assert fb2["children_total"] == "1"  # B repainted: gained the child


def test_add_child_repaints_parent_epic(tmp_graph, tmp_path):
    """codex P2: creating a child via `add --parent <epic>` repaints the epic."""
    epic, e_doc = _epic_with_plan(tmp_path, "x-0e0e", "epic")
    _seed(tmp_graph, [epic])
    res = runner.invoke(app, ["backlog", "add", "A child", "--parent", "x-0e0e"])
    assert res.exit_code == 0, res.output
    _, fe, _ = read_plan_file(e_doc)
    assert fe["children_total"] == "1"


def test_defer_undefer_roundtrip_no_verb_failure(tmp_graph, tmp_path):
    """defer + undefer both project best-effort and never fail on a live doc."""
    plan = _plan(tmp_path)
    _seed(tmp_graph, [_node(plan)])

    res = runner.invoke(app, ["backlog", "defer", "x-1234", "--reason", "later"])
    assert res.exit_code == 0, res.output
    res = runner.invoke(app, ["backlog", "undefer", "x-1234"])
    assert res.exit_code == 0, res.output


def test_add_blocker_repaints_derived_waves(tmp_graph, tmp_path):
    """AC4 e2e: `update <child> --add-blocker <sib>` repaints the epic family's
    derived waves through the real verb (edges are the sole authority)."""
    epic, e_doc = _epic_with_plan(tmp_path, "x-0e0e", "epic")
    a_doc = tmp_path / "a.md"; a_doc.write_text(_PLAN.replace("x-1234", "x-0a0a"), encoding="utf-8")
    b_doc = tmp_path / "b.md"; b_doc.write_text(_PLAN.replace("x-1234", "x-0b0b"), encoding="utf-8")
    _seed(tmp_graph, [
        epic,
        _node(a_doc, id="x-0a0a", slug="a", parent="x-0e0e"),
        _node(b_doc, id="x-0b0b", slug="b", parent="x-0e0e"),
    ])
    # Initially both siblings are wave 0.
    res = runner.invoke(app, ["backlog", "update", "x-0b0b", "--add-blocker", "x-0a0a"])
    assert res.exit_code == 0, res.output
    assert read_plan_file(a_doc)[1]["wave"] == "0"
    assert read_plan_file(b_doc)[1]["wave"] == "1"  # now blocked by its sibling
    assert read_plan_file(e_doc)[1]["waves"] == "2"
