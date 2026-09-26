"""Unit tests for step 3 of ``_repo_has_fno_activity``: the graph read
goes through the backend-correct ``read_graph`` reader, never the frozen
graph.json bytes. A node visible only to the backend reads active; an
unreachable store reads dormant."""

from __future__ import annotations

from pathlib import Path

import pytest

from fno import config_cli


def test_backend_visible_node_reads_active(tmp_path, monkeypatch):
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    # Absent from every file on disk: only the backend holds it.
    node = {"id": "probe-node", "project": "fno", "cwd": str(repo_root)}
    monkeypatch.setattr(
        "fno.graph.store.read_graph_strict", lambda *a, **k: [node]
    )
    assert config_cli._repo_has_fno_activity(repo_root, None) is True


def test_unreachable_store_reads_dormant(tmp_path, monkeypatch):
    from fno.graph.store import StoreUnavailable

    repo_root = tmp_path / "repo"
    repo_root.mkdir()

    def boom(*a, **k):
        raise StoreUnavailable("unreachable", "keeper down")

    monkeypatch.setattr("fno.graph.store.read_graph_strict", boom)
    assert config_cli._repo_has_fno_activity(repo_root, None) is False
