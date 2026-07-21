"""The asserted symmetric ``related`` edge (x-d157, Part B).

``related`` is affinity ("two sides of the same coin", "these work well
together"), distinct from ``source_node_id`` (origin) and from the computed
relatedness sidecar (regenerable, so an assertion stored there would not
survive the next build). It is navigational only: it must never reach
``_status``, dispatch eligibility, or selection order.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

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
    g.write_text(
        json.dumps(
            {"entries": [_node("x-aaaa"), _node("x-bbbb"), _node("x-cccc")]}, indent=2
        )
        + "\n"
    )
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
    entries = json.loads(g.read_text())["entries"]
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


def test_ac4_hp_related_is_symmetric_on_write_and_on_clear(tmp_graph):
    """AC4-HP: declaring on one endpoint writes the inverse; 'null' clears both."""
    assert runner.invoke(
        app, ["backlog", "update", "x-aaaa", "--related", "x-bbbb"]
    ).exit_code == 0
    assert _related(tmp_graph, "x-aaaa") == ["x-bbbb"]
    assert _related(tmp_graph, "x-bbbb") == ["x-aaaa"]

    assert runner.invoke(
        app, ["backlog", "update", "x-aaaa", "--related", "null"]
    ).exit_code == 0
    assert _related(tmp_graph, "x-aaaa") == []
    assert _related(tmp_graph, "x-bbbb") == []


def test_replace_semantics_unlink_the_dropped_peer(tmp_graph):
    """--related replaces the list, so a dropped peer loses its inverse edge too."""
    runner.invoke(app, ["backlog", "update", "x-aaaa", "--related", "x-bbbb,x-cccc"])
    assert _related(tmp_graph, "x-bbbb") == ["x-aaaa"]

    runner.invoke(app, ["backlog", "update", "x-aaaa", "--related", "x-cccc"])
    assert _related(tmp_graph, "x-aaaa") == ["x-cccc"]
    assert _related(tmp_graph, "x-bbbb") == []
    assert _related(tmp_graph, "x-cccc") == ["x-aaaa"]


def test_related_accepts_slugs_and_repeated_flags(tmp_graph):
    result = runner.invoke(
        app,
        ["backlog", "update", "x-aaaa",
         "--related", "node-x-bbbb", "--related", "x-cccc"],
    )
    assert result.exit_code == 0, result.output
    assert _related(tmp_graph, "x-aaaa") == ["x-bbbb", "x-cccc"]


# ---------------------------------------------------------------------------
# AC2-ERR / boundaries
# ---------------------------------------------------------------------------


def test_ac2_err_self_reference_is_rejected(tmp_graph):
    """AC2-ERR: a node cannot be related to itself; the list is left unchanged."""
    result = runner.invoke(
        app, ["backlog", "update", "x-aaaa", "--related", "x-aaaa"]
    )
    assert result.exit_code != 0
    assert _related(tmp_graph, "x-aaaa") == []


def test_dangling_related_id_is_rejected_and_writes_nothing(tmp_graph):
    """An unresolvable peer refuses the whole update, mirroring --source-node."""
    result = runner.invoke(
        app, ["backlog", "update", "x-aaaa", "--related", "x-bbbb,x-zzzz"]
    )
    assert result.exit_code != 0
    assert "x-zzzz" in result.output
    # The valid half of the list must not have landed either.
    assert _related(tmp_graph, "x-aaaa") == []
    assert _related(tmp_graph, "x-bbbb") == []


def test_related_is_non_blocking(tmp_graph):
    """related never gates: declaring one leaves every _status and blocked_by alone.

    Asserted as before-vs-after rather than against a literal status, so the
    test pins the invariant that matters (related does not participate in
    status derivation) instead of whatever the fixture happens to derive to.
    """
    def _statuses() -> dict[str, tuple]:
        # Read back the CANONICAL key: the writer migrates the legacy `_status`
        # to `status` and deletes it, so a round-tripped entry has only `status`.
        entries = json.loads(tmp_graph.read_text())["entries"]
        return {e["id"]: (e["status"], tuple(e["blocked_by"])) for e in entries}

    # A no-op write first, so the baseline reflects derivation, not the seed.
    runner.invoke(app, ["backlog", "update", "x-aaaa", "--related", "null"])
    before = _statuses()

    runner.invoke(app, ["backlog", "update", "x-aaaa", "--related", "x-bbbb"])
    assert _statuses() == before


# ---------------------------------------------------------------------------
# AC7-HP: filing-time related
# ---------------------------------------------------------------------------


def test_ac7_hp_related_at_filing_time(tmp_graph):
    """AC7-HP: --related on idea holds symmetry with no follow-up update."""
    result = runner.invoke(
        app, ["backlog", "idea", "co-delivered work", "--related", "x-bbbb"]
    )
    assert result.exit_code == 0, result.output
    new_id = json.loads(result.stdout)["id"]
    assert _related(tmp_graph, new_id) == ["x-bbbb"]
    assert _related(tmp_graph, "x-bbbb") == [new_id]


def test_filing_time_dangling_peer_refuses_the_whole_filing(tmp_graph):
    before = len(json.loads(tmp_graph.read_text())["entries"])
    result = runner.invoke(
        app, ["backlog", "add", "co-delivered work", "--related", "x-zzzz"]
    )
    assert result.exit_code != 0
    assert len(json.loads(tmp_graph.read_text())["entries"]) == before


# ---------------------------------------------------------------------------
# AC1-FR: the half-edge state is unreachable
# ---------------------------------------------------------------------------


def test_ac1_fr_peer_write_failure_leaves_neither_side(tmp_graph, monkeypatch):
    """AC1-FR: fault the mirror step; the declaring write must not survive alone.

    Faulted at the peer-normalization function rather than by killing the
    process, so the assertion runs against a deterministic point. Both halves
    are in one locked_mutate_graph call, so the abort is structural.
    """
    import fno.graph.store as gs

    def boom(*a, **k):
        raise RuntimeError("peer write failed")

    monkeypatch.setattr(gs, "_mirror_related", boom)

    result = runner.invoke(
        app, ["backlog", "update", "x-aaaa", "--related", "x-bbbb"]
    )
    assert result.exit_code != 0
    assert _related(tmp_graph, "x-aaaa") == []
    assert _related(tmp_graph, "x-bbbb") == []


def test_ac2_fr_opposite_endpoint_declarations_both_survive(tmp_graph):
    """AC2-FR: B keeps both A's and C's edges; neither is lost to a rewrite.

    Sequential rather than threaded: each update re-reads under the graph lock,
    so serialized writes are exactly what two concurrent sessions produce.
    """
    runner.invoke(app, ["backlog", "update", "x-aaaa", "--related", "x-bbbb"])
    runner.invoke(app, ["backlog", "update", "x-cccc", "--related", "x-bbbb"])

    assert sorted(_related(tmp_graph, "x-bbbb")) == ["x-aaaa", "x-cccc"]
    assert _related(tmp_graph, "x-aaaa") == ["x-bbbb"]
    assert _related(tmp_graph, "x-cccc") == ["x-bbbb"]


def test_removing_a_node_unlinks_it_from_every_peer(tmp_graph):
    """remove is the one path that can strand a half-edge permanently.

    set_related only touches peers in the declaring node's own delta, so a peer
    left naming a deleted node is unreachable by any repair verb.
    """
    runner.invoke(app, ["backlog", "update", "x-aaaa", "--related", "x-bbbb,x-cccc"])
    assert _related(tmp_graph, "x-bbbb") == ["x-aaaa"]

    assert runner.invoke(
        app, ["backlog", "remove", "x-aaaa", "--force"]
    ).exit_code == 0
    assert _related(tmp_graph, "x-bbbb") == []
    assert _related(tmp_graph, "x-cccc") == []


def test_archive_holds_back_a_node_an_open_peer_is_related_to():
    """A terminal node related to an OPEN node must not be swept.

    Archiving one side leaves the open node naming an id the working graph no
    longer has, with the inverse beyond set_related's reach - the symmetry
    contract broken by routine daily grooming rather than by any explicit edit.
    """
    from datetime import datetime, timedelta, timezone

    from fno.graph.archive import partition_for_archive

    now = datetime.now(timezone.utc)
    old = (now - timedelta(days=400)).isoformat()
    entries = [
        _node("x-open", status="ready", related=["x-done"]),
        _node("x-done", status="done", completed_at=old, related=["x-open"]),
        _node("x-lonely", status="done", completed_at=old),
    ]
    to_archive, _remaining, skipped = partition_for_archive(entries, 30, now)

    archived_ids = {e["id"] for e in to_archive}
    assert "x-done" not in archived_ids, "an open peer's related target is held back"
    assert "x-lonely" in archived_ids, "an unreferenced terminal node still sweeps"
    assert "x-done" in {e["id"] for e in skipped}


def test_archive_holds_back_an_open_node_s_origin():
    """An open node's source_node_id target must survive the sweep.

    This PR made the field readable, so archiving its target turns a live edge
    into a dangler the reader renders as "(not in graph)".
    """
    from datetime import datetime, timedelta, timezone

    from fno.graph.archive import partition_for_archive

    now = datetime.now(timezone.utc)
    old = (now - timedelta(days=400)).isoformat()
    entries = [
        _node("x-open", status="ready", source_node_id="x-origin"),
        _node("x-origin", status="done", completed_at=old),
    ]
    to_archive, _remaining, _skipped = partition_for_archive(entries, 30, now)
    assert "x-origin" not in {e["id"] for e in to_archive}


def test_removing_an_origin_clears_its_dependents_reference(tmp_graph):
    """remove is a HARD delete, so a dependent's origin must not dangle.

    archive keeps the node readable and guards it instead; remove cannot, so
    the stated invariant (null or resolves, never a dangling string) is held by
    clearing.
    """
    created = runner.invoke(
        app, ["backlog", "idea", "follow-up", "--source-node", "x-aaaa"]
    )
    new_id = json.loads(created.stdout)["id"]

    assert runner.invoke(
        app, ["backlog", "remove", "x-aaaa", "--force"]
    ).exit_code == 0
    entries = json.loads(tmp_graph.read_text())["entries"]
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
