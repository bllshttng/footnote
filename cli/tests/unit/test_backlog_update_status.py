"""`fno backlog update --status/--set`: the patch door, end to end (x-665f).

Every test drives the real CLI, which forwards to the native backlog-update
action, which writes through the real store - never a stubbed receipt
asserted against its own renderer. The lifecycle verbs are transports over
the same door, so the undefer/unsupersede contracts live here too.

Filter: ``fno doctor test cli/tests/unit/test_backlog_update_status.py``
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.graph.cli import cli
from fno.graph.store import commit_rows_via_store, read_graph_strict

pytestmark = pytest.mark.usefixtures("native_backlog_door")

runner = CliRunner()


def _node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "slug": f"slug-{node_id}",
        "title": f"node {node_id}",
        "project": "fno",
        "type": "feature",
        "parent": None,
        "priority": "p2",
        "status": "idea",
        "blocked_by": [],
        "completed_at": None,
        "deferred_at": None,
        "pr_number": None,
        "pr_url": None,
        "children": [],
    }
    base.update(overrides)
    return base


@pytest.fixture()
def tmp_graph(tmp_path, monkeypatch):
    from fno.graph import cli as graph_cli

    g = tmp_path / "graph.json"
    commit_rows_via_store(g, lambda entries: entries)
    monkeypatch.setattr(graph_cli, "_graph_path", lambda: g)
    return g


def _seed(g: Path, *nodes: dict) -> None:
    commit_rows_via_store(g, lambda entries: entries + list(nodes))


def _entry(g: Path, node_id: str) -> dict:
    return next(e for e in read_graph_strict(g) if e["id"] == node_id)


def _invoke(*args):
    return runner.invoke(cli, list(args), catch_exceptions=True)


# ---------------------------------------------------------------------------
# --status: the readback contract
# ---------------------------------------------------------------------------


def test_undefer_a_still_superseded_node_refuses_naming_the_door(tmp_graph):
    """AC5-HP: the false-receipt shape. A node carrying both facts cannot be
    undeferred into a lie; the refusal names the route that revives it."""
    _seed(
        tmp_graph,
        _node(
            "x-3873",
            status="superseded",
            superseded_by="x-aaaa",
            deferred_at="2026-01-01T00:00:00+00:00",
            deferred_reason="stale",
        ),
        _node("x-aaaa", supersedes=["x-3873"], status="ready"),
    )

    r = _invoke("undefer", "x-3873")

    assert r.exit_code == 2, r.output
    assert "fno backlog update x-3873 --status idea" in r.output
    assert "Undeferred" not in r.output
    node = _entry(tmp_graph, "x-3873")
    assert node["deferred_at"] == "2026-01-01T00:00:00+00:00"
    assert node["status"] == "superseded"


def test_undefer_a_plain_deferred_node_clears_and_prints_the_ack(tmp_graph):
    """AC5-ERR: a real undefer clears the facts, prints Undeferred."""
    _seed(
        tmp_graph,
        _node(
            "x-0001",
            status="deferred",
            deferred_at="2026-01-01T00:00:00+00:00",
            deferred_reason="stale",
        ),
    )

    r = _invoke("undefer", "x-0001")

    assert r.exit_code == 0, r.output
    assert "Undeferred x-0001" in r.output
    node = _entry(tmp_graph, "x-0001")
    assert not node.get("deferred_at")
    assert node["status"] == "idea"


def test_undefer_a_node_that_was_not_deferred_is_an_unchanged_no_op(tmp_graph):
    """AC5-EDGE: exit 0, the unchanged receipt, no Undeferred line."""
    _seed(tmp_graph, _node("x-0001"))

    r = _invoke("undefer", "x-0001")

    assert r.exit_code == 0, r.output
    assert "unchanged" in r.output
    assert "Undeferred" not in r.output


def test_unsupersede_lands_on_the_surviving_park(tmp_graph):
    """AC6-HP: leaving superseded keeps the deferral; the node reads deferred."""
    _seed(
        tmp_graph,
        _node(
            "x-0005",
            status="superseded",
            superseded_by="x-0004",
            supersession={"cause": "moved on"},
            deferred_at="2026-01-01T00:00:00+00:00",
            deferred_reason="waiting",
        ),
        _node("x-0004", supersedes=["x-0005"], status="ready"),
    )

    r = _invoke("unsupersede", "x-0005")

    assert r.exit_code == 0, r.output
    assert "Unsuperseded x-0005" in r.output
    assert "x-0004" in r.output
    node = _entry(tmp_graph, "x-0005")
    assert node["superseded_by"] is None
    assert node["deferred_at"] == "2026-01-01T00:00:00+00:00"
    assert node["status"] == "deferred"
    assert _entry(tmp_graph, "x-0004")["supersedes"] == []


def test_defer_and_retract_land_the_same_rows_through_the_door(tmp_graph):
    """AC7-HP: a locked node defers cleanly (lock facts clear); retract is
    defer with the kind forced; both routes share the door's validator set."""
    _seed(tmp_graph, _node("x-0003", status="ready", locked_by="sess-1", locked_at="2026-01-01T00:00:00+00:00"))

    r = _invoke("defer", "x-0003", "--reason", "waiting on x")
    assert r.exit_code == 0, r.output
    node = _entry(tmp_graph, "x-0003")
    assert node["locked_by"] is None
    assert node["locked_at"] is None
    assert node["deferred_reason"] == "waiting on x"
    assert node["status"] == "deferred"

    r = _invoke("defer", "x-0003", "--reason", "again")
    assert r.exit_code == 0, r.output
    assert _entry(tmp_graph, "x-0003")["deferred_reason"] == "again"

    r = _invoke("retract", "x-0003", "filed on a false premise")
    assert r.exit_code == 0, r.output
    node = _entry(tmp_graph, "x-0003")
    assert node["deferred_kind"] == "retracted"
    assert node["status"] == "deferred"


def test_defer_stamps_the_exact_match_kind_without_a_flag(tmp_graph):
    """The machine-stamp classifier rides the transport: the drain reason
    self-classifies as expired; prose stays unknown and clears a stale kind."""
    _seed(tmp_graph, _node("x-0001", status="ready"))

    r = _invoke("defer", "x-0001", "--reason", "stale >30d, drained by maintain")
    assert r.exit_code == 0, r.output
    assert _entry(tmp_graph, "x-0001")["deferred_kind"] == "expired"

    r = _invoke("defer", "x-0001", "--reason", "just prose")
    assert r.exit_code == 0, r.output
    node = _entry(tmp_graph, "x-0001")
    assert "deferred_kind" not in node or node["deferred_kind"] is None


