"""x-5c25: the launch edge is recorded on the NODE, not only on the registry row.

Three fields answer three different questions and only two were ever written:
``source_session_id`` is who DISCOVERED the node, ``sessions[]`` is who WORKED
it, ``spawned_by_session`` is who LAUNCHED it. The third measured 0 of 2356
nodes before this stamp existed, flat zero in every cohort.

  AC1-HP:   a spawn that names a node and proves a parent writes the triple.
  AC2-HP:   the values written are the ones the registry row carries.
  AC3-EDGE: no node, or no proven parent session, writes nothing.
  AC4-EDGE: an existing edge is never overwritten - launch is the FIRST launch.
"""
from __future__ import annotations

import pytest

from fno.agents.cli import _stamp_launch_edge


PARENT = ("parent-session-abc123", "claude", "/parent/working/dir")


@pytest.fixture
def graph(monkeypatch):
    """A one-node in-memory graph, read and mutated through the real code.

    ``_stamp_launch_edge`` imports both store functions lazily, so patching the
    store module is enough. The keeper subprocess a real write would need buys
    nothing here: the assertions are the skip decision and what the mutator
    does to the row.
    """
    import fno.graph.store as store

    entries = [{"id": "x-1234", "title": "a node", "status": "ready"}]
    calls: list[int] = []

    def fake_mutate(path, mutator):
        calls.append(1)
        return mutator(entries)

    monkeypatch.setattr(store, "read_graph", lambda *a, **k: entries)
    monkeypatch.setattr(store, "locked_mutate_graph", fake_mutate)
    return entries, calls


@pytest.fixture
def parent(monkeypatch):
    """Pin the ambient parent edge so the test never reads the real session."""
    import fno.agents.dispatch as dispatch

    monkeypatch.setattr(dispatch, "_capture_parent_edge", lambda: PARENT)


def test_ac1_hp_launch_edge_lands_on_the_node(graph, parent):
    """AC1-HP + AC2-HP: the node carries the same triple the row carries."""
    entries, calls = graph

    _stamp_launch_edge("x-1234")

    assert calls == [1], "expected exactly one graph write"
    row = entries[0]
    assert row["spawned_by_session"] == "parent-session-abc123"
    assert row["spawned_by_harness"] == "claude"
    assert row["spawned_by_cwd"] == "/parent/working/dir"


def test_ac3_edge_no_node_writes_nothing(graph, parent):
    """AC3-EDGE: an ad-hoc spawn names no node, so there is nothing to stamp."""
    entries, calls = graph

    _stamp_launch_edge(None)
    _stamp_launch_edge("")

    assert calls == [], "a node-less spawn must not touch the graph"
    assert "spawned_by_session" not in entries[0]


def test_ac3_edge_unproven_parent_writes_nothing(graph, monkeypatch):
    """AC3-EDGE: a null session on a durable node would assert an untraceable
    launch. The registry row and its agent_spawned event already record the
    absence with its reason, so the node stays silent."""
    entries, calls = graph
    import fno.agents.dispatch as dispatch

    monkeypatch.setattr(dispatch, "_capture_parent_edge", lambda: (None, "claude", "/cwd"))

    _stamp_launch_edge("x-1234")

    assert calls == [], "an unproven parent must not half-write the edge"
    assert "spawned_by_session" not in entries[0]


def test_ac4_edge_existing_edge_is_never_overwritten(graph, parent, capsys):
    """AC4-EDGE: a second worker on the node does not rewrite who started it,
    and does not pay a locked write to discover that."""
    entries, calls = graph
    entries[0].update(
        spawned_by_session="the-first-launcher",
        spawned_by_harness="codex",
        spawned_by_cwd="/first/cwd",
    )

    _stamp_launch_edge("x-1234")

    assert calls == [], "a settled edge must not pay a locked write"
    assert entries[0]["spawned_by_session"] == "the-first-launcher"
    assert entries[0]["spawned_by_harness"] == "codex"
    assert entries[0]["spawned_by_cwd"] == "/first/cwd"
    assert "already names the-first-launcher; kept" in capsys.readouterr().err


def test_a_racing_first_launch_still_wins_under_the_lock(graph, parent):
    """The pre-read is a snapshot. A launch that lands between the read and the
    commit is the first launch, and the mutator re-checks rather than clobber."""
    entries, _calls = graph
    import fno.graph.store as store

    def racing_mutate(path, mutator):
        entries[0]["spawned_by_session"] = "the-racing-launcher"
        return mutator(entries)

    store.locked_mutate_graph = racing_mutate

    _stamp_launch_edge("x-1234")

    assert entries[0]["spawned_by_session"] == "the-racing-launcher"


def test_a_node_the_graph_does_not_hold_says_so(graph, parent, capsys):
    """A stamp that writes nothing must SAY so.

    resolve_provenance keeps a well-formed id it could not resolve, so without
    this the spawn would fall silent and a reader could not tell a missing node
    from a spawn that had no parent.
    """
    _entries, calls = graph

    _stamp_launch_edge("x-9999")

    assert calls == [], "an unknown node must not pay a locked write"
    assert "not recorded on x-9999 (node not in graph)" in capsys.readouterr().err


def test_a_graph_failure_never_fails_the_spawn(monkeypatch, parent, capsys):
    """The stamp is provenance, and provenance never costs a worker."""
    import fno.graph.store as store

    def boom(*a, **k):
        raise RuntimeError("graph keeper is wedged")

    monkeypatch.setattr(store, "read_graph", boom)

    _stamp_launch_edge("x-1234")

    assert "launch edge not recorded on x-1234" in capsys.readouterr().err
