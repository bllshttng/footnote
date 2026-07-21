"""Tournament ordering for backlog triage.

Comparative judgment (pairwise "ship X or Y first?") is more reliable than
one-shot absolute scoring for qualitative ranking. `_copeland_rank` folds the
pairwise verdicts into one stable order; `fno backlog triage rank` wires it.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.graph.triage import _copeland_rank

runner = CliRunner()


def test_copeland_orders_by_net_score():
    ids = ["a", "b", "c"]
    verdicts = [
        {"winner": "a", "loser": "b"},
        {"winner": "a", "loser": "c"},
        {"winner": "b", "loser": "c"},
    ]
    assert [r["id"] for r in _copeland_rank(ids, verdicts)] == ["a", "b", "c"]


def test_copeland_tolerates_contradiction_and_uses_meta_tiebreak():
    # One win each -> net 0 for both; meta breaks the tie (a has better prio).
    verdicts = [{"winner": "a", "loser": "b"}, {"winner": "b", "loser": "a"}]
    meta = {"a": (1, ""), "b": (2, "")}
    assert [r["id"] for r in _copeland_rank(["a", "b"], verdicts, meta)] == ["a", "b"]


def test_copeland_ignores_unknown_ids():
    assert _copeland_rank(["a"], [{"winner": "a", "loser": "ghost"}]) == [
        {"id": "a", "wins": 0, "losses": 0, "net": 0}
    ]


def test_copeland_tolerates_non_string_verdict_values():
    # Malformed JSON could carry a list/dict (unhashable) winner/loser; the
    # isinstance guard must skip it rather than raise TypeError.
    verdicts = [{"winner": ["x"], "loser": "a"}, {"winner": "a", "loser": "b"}]
    assert [r["id"] for r in _copeland_rank(["a", "b"], verdicts)] == ["a", "b"]


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    return g


def test_triage_rank_cli_round_trip(tmp_graph, tmp_path):
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "ab-1", "title": "one", "priority": "p2"},
        {"id": "ab-2", "title": "two", "priority": "p2"},
    ]}))
    vfile = tmp_path / "verdicts.json"
    vfile.write_text(json.dumps([{"winner": "ab-2", "loser": "ab-1"}]))
    r = runner.invoke(app, ["backlog", "triage", "rank", "--verdicts", str(vfile)],
                      catch_exceptions=False)
    assert r.exit_code == 0, r.output
    order = json.loads(r.stdout)["order"]
    assert [o["id"] for o in order] == ["ab-2", "ab-1"]
    assert order[0]["title"] == "two"


def test_triage_rank_drops_ids_not_in_graph(tmp_graph, tmp_path):
    # A stale / typo'd verdict id must not reach the emitted order (it would
    # then fail `fno backlog rank`). Only graph-resident ids participate.
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "ab-1", "title": "one", "priority": "p2"},
    ]}))
    vfile = tmp_path / "verdicts.json"
    vfile.write_text(json.dumps([{"winner": "ab-1", "loser": "ab-GONE"}]))
    r = runner.invoke(app, ["backlog", "triage", "rank", "--verdicts", str(vfile)],
                      catch_exceptions=False)
    assert r.exit_code == 0, r.output
    order = json.loads(r.stdout)["order"]
    assert [o["id"] for o in order] == ["ab-1"]
