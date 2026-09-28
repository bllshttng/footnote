"""The strict graph reader distinguishes populated, empty, and unreadable stores."""
from __future__ import annotations

from pathlib import Path

import pytest
from tests.fixtures.graph_seed import seed_graph

from fno.graph.store import GraphUnreadableError, read_graph_strict


def test_populated_store_returns_entries(tmp_path: Path):
    graph = tmp_path / "graph.json"
    seed_graph(graph, [{"id": "x-aaaa", "title": "A"}])
    assert [entry["id"] for entry in read_graph_strict(graph)] == ["x-aaaa"]


def test_empty_store_returns_empty_quietly(tmp_path: Path):
    graph = tmp_path / "graph.json"
    seed_graph(graph, [])
    assert read_graph_strict(graph) == []


def test_corrupt_store_raises_not_returns_empty(tmp_path: Path):
    graph = tmp_path / "graph.json"
    graph.with_suffix(".db").write_bytes(b"not sqlite")
    with pytest.raises(GraphUnreadableError):
        read_graph_strict(graph)


def test_unreadable_store_names_its_database_path(tmp_path: Path):
    graph = tmp_path / "graph.json"
    database = graph.with_suffix(".db")
    database.write_bytes(b"not sqlite")
    with pytest.raises(GraphUnreadableError) as exc:
        read_graph_strict(graph)
    assert str(database) in str(exc.value)


def test_absent_store_returns_empty(tmp_path: Path):
    assert read_graph_strict(tmp_path / "does-not-exist.json") == []
