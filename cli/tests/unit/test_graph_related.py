"""The asserted symmetric ``related`` edge (x-d157, Part B).

``related`` is affinity ("two sides of the same coin", "these work well
together"), distinct from ``source_node_id`` (origin) and from the computed
relatedness sidecar (regenerable, so an assertion stored there would not
survive the next build). It is navigational only: it must never reach
``_status``, dispatch eligibility, or selection order.
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.rust_binary import find_dev_binary
from fno.graph.store import read_graph_strict

# Since the store port every test here rides the keeper, so the module needs
# the compiled runtime and skips whole where the smoke harness deleted the
# worker binary (the parity-test convention).
requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)

pytestmark = requires_rust

runner = CliRunner()


def _node(node_id: str, **over) -> dict:
    base = {
        "id": node_id,
        "title": f"Node {node_id}",
        "_status": "ready",
        "domain": "code",
        "project": "fno",
        "slug": f"node-{node_id}",
    }
    base.update(over)
    return base


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    seed_graph(g, json.dumps(
            {"entries": [_node("x-aaaa"), _node("x-bbbb"), _node("x-cccc")]}, indent=2
        )
        + "\n")
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    for var in ("FNO_NODE", "CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    return g


def _related(g: Path, node_id: str) -> list[str]:
    entries = read_graph_strict(g)
    return next(e for e in entries if e["id"] == node_id).get("related", [])


# ---------------------------------------------------------------------------
# Schema: the three edits a graph.json field needs
# ---------------------------------------------------------------------------


def test_related_defaults_to_empty_on_a_legacy_node():
    """A node written before the field existed reads back as [], never missing."""
    from fno.graph.store import _apply_graph_defaults

    (e,) = _apply_graph_defaults([_node("x-legacy")])
    assert e["related"] == []


def test_related_is_ordered_with_the_other_edges():
    """related sits in CANONICAL_FIELD_ORDER, next to blocked_by.

    Asserting POSITION, not presence: canonicalize_entries appends unknown keys
    rather than dropping them, so a presence check passes even with the field
    absent from the order list and proves nothing.
    """
    from fno.graph.store import canonicalize_entries

    (e,) = canonicalize_entries(
        [_node("x-aaaa", related=["x-bbbb"], blocked_by=[], created_at="2026-01-01")]
    )
    keys = list(e)
    assert e["related"] == ["x-bbbb"]
    assert keys.index("blocked_by") < keys.index("related") < keys.index("created_at")


# ---------------------------------------------------------------------------
# AC4-HP: symmetry on write
# ---------------------------------------------------------------------------


def test_set_related_keeps_held_row_references_live(monkeypatch):
    """A mutator holding a node dict keeps writing to the row that persists."""
    import fno.graph.store as gs

    monkeypatch.setattr(
        gs, "_pure",
        lambda entries, name, params: [dict(e, related=["x-bbbb"]) for e in entries],
    )
    entries = [_node("x-aaaa")]
    held = entries[0]
    gs.set_related(entries, "x-aaaa", ["x-bbbb"])
    held["details"] = "marker-5934"
    assert entries[0]["details"] == "marker-5934"
    assert entries[0]["related"] == ["x-bbbb"]


def test_ac7_hp_related_at_filing_time(tmp_graph):
    """AC7-HP: --related on idea holds symmetry with no follow-up update."""
    result = runner.invoke(
        app, ["backlog", "idea", "co-delivered work", "--related", "x-bbbb", "--difficulty", "low"]
    )
    assert result.exit_code == 0, result.output
    new_id = json.loads(result.stdout)["id"]
    assert _related(tmp_graph, new_id) == ["x-bbbb"]
    assert _related(tmp_graph, "x-bbbb") == [new_id]


def test_filing_time_dangling_peer_refuses_the_whole_filing(tmp_graph):
    before = len(read_graph_strict(tmp_graph))
    result = runner.invoke(
        app, ["backlog", "add", "co-delivered work", "--related", "x-zzzz", "--difficulty", "medium"]
    )
    assert result.exit_code != 0
    assert len(read_graph_strict(tmp_graph)) == before


# ---------------------------------------------------------------------------
# AC1-FR: the half-edge state is unreachable
# ---------------------------------------------------------------------------


def test_removing_a_node_unlinks_it_from_every_peer(tmp_graph):
    """remove is the one path that can strand a half-edge permanently.

    set_related only touches peers in the declaring node's own delta, so a peer
    left naming a deleted node is unreachable by any repair verb.
    """
    seed_graph(tmp_graph, json.dumps({"entries": [
        dict(_node("x-aaaa"), related=["x-bbbb", "x-cccc"]),
        dict(_node("x-bbbb"), related=["x-aaaa"]),
        dict(_node("x-cccc"), related=["x-aaaa"]),
    ]}, indent=2) + "\n")
    assert _related(tmp_graph, "x-bbbb") == ["x-aaaa"]

    assert runner.invoke(
        app, ["backlog", "remove", "x-aaaa", "--force"]
    ).exit_code == 0
    assert _related(tmp_graph, "x-bbbb") == []
    assert _related(tmp_graph, "x-cccc") == []


def test_archive_releases_an_open_peer_s_related_edge():
    """A terminal node related to an OPEN node is swept, its edge stripped.

    A related edge is soft: it pins finished work in the working set forever
    when it holds (203 nodes on the machine this was measured on), so the sweep
    releases it on the open side instead of guarding the target. Read-through
    keeps the archived id resolvable, and release_soft_edges is what strips
    the open side at apply time - this partition test pins the drain half.
    """
    from datetime import datetime, timedelta, timezone

    from fno.graph.archive import partition_for_archive, release_soft_edges

    now = datetime.now(timezone.utc)
    old = (now - timedelta(days=400)).isoformat()
    entries = [
        _node("x-open", status="ready", related=["x-done"]),
        _node("x-done", status="done", completed_at=old, related=["x-open"]),
        _node("x-lonely", status="done", completed_at=old),
    ]
    to_archive, remaining, skipped = partition_for_archive(entries, 30, now)

    archived_ids = {e["id"] for e in to_archive}
    assert "x-done" in archived_ids, "an open peer's related target now drains"
    assert "x-lonely" in archived_ids, "an unreferenced terminal node still sweeps"
    assert "x-done" not in {e["id"] for e in skipped}, "nothing held it back"

    patched, stripped = release_soft_edges(remaining, archived_ids)
    open_node = next(e for e in patched if e["id"] == "x-open")
    assert open_node["related"] == [], "the open side no longer names the archived id"
    assert stripped == 1


def test_archive_releases_an_open_node_s_origin_edge():
    """An open node's source_node_id target drains too; the origin is nulled.

    The field stays readable: a nulled origin is a clean "no origin", and the
    archived target itself stays resolvable through read-through by id.
    """
    from datetime import datetime, timedelta, timezone

    from fno.graph.archive import partition_for_archive, release_soft_edges

    now = datetime.now(timezone.utc)
    old = (now - timedelta(days=400)).isoformat()
    entries = [
        _node("x-open", status="ready", source_node_id="x-origin"),
        _node("x-origin", status="done", completed_at=old),
    ]
    to_archive, remaining, _skipped = partition_for_archive(entries, 30, now)
    assert "x-origin" in {e["id"] for e in to_archive}
    patched, stripped = release_soft_edges(remaining, {e["id"] for e in to_archive})
    open_node = next(e for e in patched if e["id"] == "x-open")
    assert open_node["source_node_id"] is None
    assert stripped == 1


def test_removing_an_origin_clears_its_dependents_reference(tmp_graph):
    """remove is a HARD delete, so a dependent's origin must not dangle.

    archive keeps the node readable and guards it instead; remove cannot, so
    the stated invariant (null or resolves, never a dangling string) is held by
    clearing.
    """
    created = runner.invoke(
        app, ["backlog", "idea", "follow-up", "--source-node", "x-aaaa", "--difficulty", "low"]
    )
    new_id = json.loads(created.stdout)["id"]

    assert runner.invoke(
        app, ["backlog", "remove", "x-aaaa", "--force"]
    ).exit_code == 0
    entries = read_graph_strict(tmp_graph)
    node = next(e for e in entries if e["id"] == new_id)
    assert node["source_node_id"] is None


def test_two_terminal_related_peers_are_swept_together_or_not_at_all():
    """A related pair must not split across the archive boundary.

    _guard_ids only protects references held by OPEN nodes, so two terminal
    peers of different ages would otherwise split: the older sweeps and the
    newer stays behind naming an id the working graph no longer has, which
    set_related cannot repair since it resolves peers against that graph.
    """
    from datetime import datetime, timedelta, timezone

    from fno.graph.archive import partition_for_archive

    now = datetime.now(timezone.utc)
    old = (now - timedelta(days=400)).isoformat()
    recent = (now - timedelta(days=2)).isoformat()

    entries = [
        _node("x-old", status="done", completed_at=old, related=["x-new"]),
        _node("x-new", status="done", completed_at=recent, related=["x-old"]),
    ]
    to_archive, _remaining, skipped = partition_for_archive(entries, 30, now)
    assert to_archive == [], "the old peer waits for its partner"
    assert "related-peer-not-archived" in {e.get("_skip") for e in skipped}

    # Once both are old enough, they move together and the edge stays intact.
    entries[1]["completed_at"] = old
    to_archive, _remaining, _skipped = partition_for_archive(entries, 30, now)
    assert {e["id"] for e in to_archive} == {"x-old", "x-new"}


def test_a_related_chain_holds_back_transitively():
    """Holding one node back can strand the next; the fixed point must catch it."""
    from datetime import datetime, timedelta, timezone

    from fno.graph.archive import partition_for_archive

    now = datetime.now(timezone.utc)
    old = (now - timedelta(days=400)).isoformat()
    recent = (now - timedelta(days=2)).isoformat()

    entries = [
        _node("x-a", status="done", completed_at=old, related=["x-b"]),
        _node("x-b", status="done", completed_at=old, related=["x-a", "x-c"]),
        _node("x-c", status="done", completed_at=recent, related=["x-b"]),
    ]
    to_archive, _remaining, _skipped = partition_for_archive(entries, 30, now)
    assert to_archive == [], "b waits on c, and a waits on b"


# ---------------------------------------------------------------------------
# x-129e: --related alongside other flags in one `update` call
# ---------------------------------------------------------------------------


