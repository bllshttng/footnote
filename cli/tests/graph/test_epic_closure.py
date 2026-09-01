"""Epic-closure rules: exactly the two exceptions flip stuck epics closable.

The flip proof (AC10 of the plan): on the six-epic shape, strict done-only
closure closes NOTHING, and adding exactly the two rules (superseded and
wont_do never hold an epic open) closes EXACTLY the trio. All fixtures are
synthetic; the real-graph live check lives at the bottom and skips anywhere
but the migration host.

Filter: ``fno doctor test cli/tests/graph/test_epic_closure.py``
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone

import pytest
from typer.testing import CliRunner

import fno.graph.epics as epics_module
from fno.graph.cli import cli
from fno.graph.epics import holds_epic_open, stuck_epics
from fno.graph.store import locked_mutate_graph

runner = CliRunner()

NOW = datetime(2026, 9, 1, 12, 0, 0, tzinfo=timezone.utc)


def _n(node_id, status, kind=None, children=None, parent=None):
    return {
        "id": node_id,
        "title": f"node {node_id}",
        "project": "fno",
        "type": "epic" if children is not None else "feature",
        "status": status,
        "deferred_kind": kind,
        "deferred_at": "2026-08-01T00:00:00+00:00" if status == "deferred" else None,
        "deferred_reason": f"reason for {node_id}" if status == "deferred" else None,
        "priority": "p2",
        "parent": parent,
        "children": [{"id": c, "title": None, "project": None, "status": None} for c in (children or [])],
        "blocked_by": [],
        "completed_at": None if status != "done" else (NOW - timedelta(days=1)).isoformat(),
        "pr_number": None,
        "pr_url": None,
        "created_at": (NOW - timedelta(days=2)).isoformat(),
    }


@pytest.fixture()
def six_epic_graph():
    """The measured shape of the six stuck epics + one advancing control."""
    nodes = [
        # trio closable under the two rules
        _n("ep-x73", "in_progress", children=["c1", "c2", "c3"]),
        _n("c1", "done", parent="ep-x73"),
        _n("c2", "superseded", parent="ep-x73"),
        _n("c3", "deferred", kind="wont_do", parent="ep-x73"),
        _n("ep-c42", "in_progress", children=["d1", "d2"]),
        _n("d1", "done", parent="ep-c42"),
        _n("d2", "deferred", kind="wont_do", parent="ep-c42"),
        _n("ep-897", "in_progress", children=["f1", "f2"]),
        _n("f1", "done", parent="ep-897"),
        _n("f2", "superseded", parent="ep-897"),
        # held open
        _n("ep-c1b", "in_progress", children=["g1", "g2"]),
        _n("g1", "done", parent="ep-c1b"),
        _n("g2", "deferred", kind=None, parent="ep-c1b"),  # unclassified
        _n("ep-522", "in_progress", children=["h1", "h2"]),
        _n("h1", "done", parent="ep-522"),
        _n("h2", "deferred", kind="contingent", parent="ep-522"),
        # all children resolved but the EPIC itself deferred
        _n("ep-ad5", "deferred", children=["i1", "i2"]),
        _n("i1", "done", parent="ep-ad5"),
        _n("i2", "superseded", parent="ep-ad5"),
        # control: advancing work means not stuck
        _n("ep-go", "in_progress", children=["j1", "j2"]),
        _n("j1", "done", parent="ep-go"),
        _n("j2", "in_progress", parent="ep-go"),
    ]
    # children summaries carry the derived status, same as the real store
    by_id = {n["id"]: n for n in nodes}
    for n in nodes:
        for k in n["children"]:
            k["status"] = by_id[k["id"]]["status"]
    return nodes


def test_trio_closable_others_not(six_epic_graph):
    rows = {r.id: r for r in stuck_epics(six_epic_graph)}
    assert set(rows) == {"ep-x73", "ep-c42", "ep-897", "ep-c1b", "ep-522", "ep-ad5"}
    assert {r.id for r in rows.values() if r.closable} == {"ep-x73", "ep-c42", "ep-897"}
    assert rows["ep-c1b"].held_open_by == ["g2"]
    assert rows["ep-522"].held_open_by == ["h2"]
    assert rows["ep-ad5"].holders == [] and not rows["ep-ad5"].closable


def test_the_two_rules_are_what_flip_the_trio(six_epic_graph, monkeypatch):
    monkeypatch.setattr(
        epics_module, "holds_epic_open", lambda child: child.get("status") != "done"
    )
    rows = stuck_epics(six_epic_graph)
    assert not any(r.closable for r in rows), "strict done-only must close nothing here"


def test_wont_do_holds_but_superseded_does_not():
    assert not holds_epic_open({"id": "a", "status": "done"})
    assert not holds_epic_open({"id": "a", "status": "superseded"})
    assert not holds_epic_open({"id": "a", "status": "deferred", "deferred_kind": "wont_do"})
    for kind in (None, "expired", "contingent", "blocked"):
        assert holds_epic_open({"id": "a", "status": "deferred", "deferred_kind": kind})


def test_stuck_epics_verb_reports_and_never_mutates(tmp_path, monkeypatch):
    from fno.graph import cli as graph_cli

    g = tmp_path / "graph.json"
    locked_mutate_graph(
        g,
        lambda entries: entries
        + [
            _n("ep-1", "in_progress", children=["k1", "k2"]),
            _n("k1", "done", parent="ep-1"),
            _n("k2", "deferred", kind="wont_do", parent="ep-1"),
        ],
    )
    monkeypatch.setattr(graph_cli, "_graph_path", lambda: g)
    r = runner.invoke(cli, ["stuck-epics", "-J"])
    assert r.exit_code == 0, r.output
    report = json.loads(r.output)
    assert report["stuck_epics"][0]["id"] == "ep-1"
    assert report["stuck_epics"][0]["closable"] is True
