"""FTS5 search cache: hash-gated, full-rebuild-only, never stale-confident.

The positive markers under test: a hash mismatch FORCES a visible rebuild
(content change is readable through the same search that was stale-bound),
and a hash HIT provably skips the rebuild (mutation counter stays at zero).
No fixture builds its own index and asserts against it; every read routes
through ``fts.search`` and every index through ``ensure_search_index``.

Filter: ``fno doctor test cli/tests/graph/test_backlog_search_fts.py``
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.graph import fts
from fno.graph.cli import cli
from fno.graph.store import locked_mutate_graph

runner = CliRunner()

fts5 = pytest.mark.skipif(
    not fts._fts5_supported(), reason="FTS5 unavailable in this sqlite build"
)


def _node(node_id, **overrides):
    base = {
        "id": node_id,
        "title": f"node {node_id}",
        "project": "fno",
        "type": "feature",
        "parent": None,
        "priority": "p2",
        "status": "ready",
        "blocked_by": [],
        "completed_at": None,
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
            _node("x-aaaa", title="resume handle provenance join", details="the ledger stores session uuids"),
            _node("x-bbbb", title="unrelated work item"),
        ],
    )
    # two seams: the verbs mutate via _graph_path; `find`'s display reader
    # resolves through paths.graph_json (both documented test redirects)
    monkeypatch.setattr(graph_cli, "_graph_path", lambda: g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


@fts5
def test_hash_hit_skips_rebuild(tmp_graph):
    fts.ensure_search_index(tmp_graph)
    with _counting_rebuilds(tmp_graph) as counter:
        fts.ensure_search_index(tmp_graph)
        fts.search("resume handle", tmp_graph)
    assert counter["rebuilds"] == 0, "a current cache must not rebuild"


@fts5
def test_hash_mismatch_forces_full_rebuild(tmp_graph):
    fts.ensure_search_index(tmp_graph)
    # mutate through the real writer: new bytes, same path
    locked_mutate_graph(
        tmp_graph, lambda entries: entries + [_node("x-cccc", title="brand new searchable thing")]
    )
    with _counting_rebuilds(tmp_graph) as counter:
        hits = fts.search("brand new searchable thing", tmp_graph)
    assert counter["rebuilds"] == 1, "changed graph bytes must force a rebuild"
    assert hits == ["x-cccc"], "the rebuilt index must answer with the NEW content"


@fts5
def test_corrupt_cache_is_a_miss_not_a_crash(tmp_graph):
    fts.ensure_search_index(tmp_graph)
    p = fts.index_path(tmp_graph)
    p.write_bytes(b"not a database at all")
    assert fts.search("resume handle", tmp_graph) == ["x-aaaa"]


@fts5
def test_query_syntax_is_neutralized(tmp_graph):
    fts.ensure_search_index(tmp_graph)
    # unbalanced quotes / fts operators must not raise or inject syntax
    assert fts.search('"unbalanced quote AND (', tmp_graph) == []


def test_find_fts_flag_ranks_and_falls_back(tmp_graph, monkeypatch):
    r = runner.invoke(cli, ["find", "--fts", "resume handle", "-J"])
    assert r.exit_code == 0, r.output
    ids = [e["id"] for e in json.loads(r.output)]
    assert ids == ["x-aaaa"]
    # an fts failure degrades to the substring lane with a warning, not an exit
    def _boom(*a, **k):
        raise fts.SearchUnavailableError("no fts5 here")

    monkeypatch.setattr(fts, "search", _boom)
    r2 = runner.invoke(cli, ["find", "--fts", "resume handle", "-J"])
    assert r2.exit_code == 0, r2.output
    assert "warning: fts unavailable" in r2.output
    # stderr warning and stdout JSON share one captured stream here
    body = r2.output[r2.output.index("[") :]
    assert [e["id"] for e in json.loads(body)] == ["x-aaaa"]


class _counting_rebuilds:
    """Count real rebuilds by monkeypatching _rebuild on the module."""

    def __init__(self, graph_path):
        self.counter = {"rebuilds": 0}
        self.graph_path = graph_path

    def __enter__(self):
        self._orig = fts._rebuild

        def counting(graph_path, dest, graph_hash):
            self.counter["rebuilds"] += 1
            return self._orig(graph_path, dest, graph_hash)

        fts._rebuild = counting
        return self.counter

    def __exit__(self, *exc):
        fts._rebuild = self._orig
        return False
