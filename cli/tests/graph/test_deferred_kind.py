"""deferred_kind: exact-string classification, verb stamping/clearing, backfill.

The guarantee under test is the exact-match contract: a node is classified
ONLY when its deferred_reason equals a known exact string, byte for byte.
Anything else stays unclassified, because a wrong kind silently changes
whether an epic can close (graph/epics.py). Every write goes through the real
verbs and every read back through ``read_graph`` - never a hand-built
summary asserted against its own renderer (the CI-green-but-changed-nothing
failure this suite exists to prevent).

Filter: ``fno doctor test cli/tests/graph/test_deferred_kind.py``
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.graph._constants import DEFERRED_KINDS, classify_deferred_reason
from fno.graph.cli import cli
from fno.graph.store import locked_mutate_graph, read_graph

runner = CliRunner()

DRAIN_REASON = "stale >30d, drained by maintain"


def _node(node_id: str, **overrides) -> dict:
    status = overrides.get("status", "ready")
    base = {
        "id": node_id,
        "title": f"node {node_id}",
        "project": "fno",
        "type": "feature",
        "parent": None,
        "priority": "p2",
        "status": status,
        "blocked_by": [],
        "completed_at": None,
        "deferred_at": "2026-08-01T00:00:00+00:00" if status == "deferred" else None,
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
    locked_mutate_graph(
        g,
        lambda entries: entries
        + [
            _node("x-aaaa", status="deferred", deferred_reason=DRAIN_REASON),
            _node("x-bbbb", status="deferred", deferred_reason="Operator 2026-08-28: over-engineered, cut before it shipped."),
            _node("x-cccc", status="deferred", deferred_reason="a prose reason nobody classified"),
            _node("x-dddd", status="ready"),
        ],
    )
    monkeypatch.setattr(graph_cli, "_graph_path", lambda: g)
    return g


def test_exact_match_classifies_drain_and_nearmiss_stays_unknown():
    assert classify_deferred_reason(DRAIN_REASON) == "expired"
    assert classify_deferred_reason("stale >31d, drained by maintain") is None
    # one trailing space is a different string: the drain interpolates the
    # configured staleness threshold, so normalization here would mislabel
    # whole fleets on non-default configs
    assert classify_deferred_reason(DRAIN_REASON + " ") is None
    assert classify_deferred_reason(None) is None
    assert classify_deferred_reason("x", {"x": "junk"}) == "junk"


def test_defer_kind_roundtrips_through_read_graph(tmp_graph):
    r = runner.invoke(cli, ["defer", "x-dddd", "-R", "custom prose", "--kind", "wont_do"])
    assert r.exit_code == 0, r.output
    node = {e["id"]: e for e in read_graph(tmp_graph)}["x-dddd"]
    assert node["deferred_kind"] == "wont_do"
    assert node["deferred_reason"] == "custom prose"


def test_defer_without_kind_classifies_exact_match_only(tmp_graph):
    r = runner.invoke(cli, ["defer", "x-dddd", "-R", DRAIN_REASON])
    assert r.exit_code == 0, r.output
    node = {e["id"]: e for e in read_graph(tmp_graph)}["x-dddd"]
    assert node["deferred_kind"] == "expired"
    r = runner.invoke(cli, ["defer", "x-cccc", "-R", "fresh unclassifiable prose"])
    assert r.exit_code == 0, r.output
    node = {e["id"]: e for e in read_graph(tmp_graph)}["x-cccc"]
    assert "deferred_kind" not in node


def test_defer_rejects_unknown_kind(tmp_graph):
    r = runner.invoke(cli, ["defer", "x-dddd", "-R", "reason", "--kind", "nope"])
    assert r.exit_code == 1
    assert ", ".join(DEFERRED_KINDS)[:20] in r.output
    # the refusal must precede any mutation: x-dddd never lands in deferred
    assert {e["id"]: e for e in read_graph(tmp_graph)}["x-dddd"].get("status") != "deferred"


def test_undefer_clears_the_kind(tmp_graph):
    r = runner.invoke(cli, ["defer", "x-dddd", "-R", "reason", "--kind", "later"])
    assert r.exit_code == 0, r.output
    r = runner.invoke(cli, ["undefer", "x-dddd"])
    assert r.exit_code == 0, r.output
    node = {e["id"]: e for e in read_graph(tmp_graph)}["x-dddd"]
    assert node.get("deferred_kind") is None and node["deferred_at"] is None


def test_backfill_dry_run_counts_and_writes_nothing(tmp_graph):
    r = runner.invoke(
        cli,
        ["backfill-deferred-kind", "-J", "--map", str(_write_map(tmp_graph))],
    )
    assert r.exit_code == 0, r.output
    report = json.loads(r.output)
    assert report["dry_run"] is True
    assert report["would_stamp"] == {"expired": 1, "wont_do": 1}
    assert report["unclassified"] == 1
    rows = {e["id"]: e for e in read_graph(tmp_graph)}
    assert "deferred_kind" not in rows["x-aaaa"]


def test_backfill_apply_is_exact_and_idempotent(tmp_graph):
    map_file = _write_map(tmp_graph)
    r = runner.invoke(cli, ["backfill-deferred-kind", "-J", "--map", str(map_file), "--apply"])
    assert r.exit_code == 0, r.output
    rows = {e["id"]: e for e in read_graph(tmp_graph)}
    assert rows["x-aaaa"]["deferred_kind"] == "expired"  # code table
    assert rows["x-bbbb"]["deferred_kind"] == "wont_do"  # map
    assert "deferred_kind" not in rows["x-cccc"]  # honest unknown
    assert rows["x-dddd"].get("deferred_kind") is None  # never non-deferred
    # idempotent: a second run stamps 0
    r2 = runner.invoke(cli, ["backfill-deferred-kind", "-J", "--map", str(map_file), "--apply"])
    assert json.loads(r2.output)["stamped"] == {}


def test_backfill_refuses_bad_map_before_touching_graph(tmp_graph):
    bad = tmp_graph.parent / "bad.tsv"
    bad.write_text("wont_do\toperator prose\nbogus_kind\tsome reason\n")
    r = runner.invoke(cli, ["backfill-deferred-kind", "--map", str(bad), "--apply"])
    assert r.exit_code == 1
    assert "bogus_kind" in r.output
    assert {e["id"]: e for e in read_graph(tmp_graph)}["x-aaaa"].get("deferred_kind") is None


def _write_map(graph: Path):
    m = graph.parent / "map.tsv"
    m.write_text(
        "# kind<TAB>exact reason\n"
        "wont_do\tOperator 2026-08-28: over-engineered, cut before it shipped.\n"
    )
    return m
