"""Archive residency lives in the same store as the working graph (task
15.1): the sweep stamps ``archived_at`` on the node rows, default reads
filter them, ``export --now`` rebuilds the advisory file, the one-shot
import folds a legacy sibling archive in, and unarchive clears the stamp.
The Python seam runs against a real keeper on a temp graph; the operator's
live graph and config are never touched."""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from tests.fixtures.graph_seed import seed_graph

FULL = {
    "type": "feature",
    "status": "idea",
    "priority": "p2",
    "domain": "code",
    "created_at": "2026-09-11T00:00:00+00:00",
}


def _row(node_id: str, **overrides):
    return {
        **FULL,
        "id": node_id,
        "slug": node_id,
        "title": node_id,
        "tags": [],
        **overrides,
    }


def _archived_at(graph: Path, node_id: str):
    with sqlite3.connect(graph.with_suffix(".db")) as connection:
        row = connection.execute(
            "SELECT archived_at FROM nodes WHERE id = ?", (node_id,)
        ).fetchone()
    return None if row is None else row[0]


def _seed(graph: Path, *rows) -> None:
    seed_graph(graph, rows)


@pytest.fixture
def world(tmp_path, monkeypatch):
    """A temp machine: graph + state root. Skips where no keeper binary
    can spawn."""
    from fno.graph.store import _worker_binary

    graph = tmp_path / "graph.json"
    _seed(
        graph,
        _row(
            "x-old",
            status="done",
            completed_at="2026-06-01T00:00:00Z",
            title="old done",
        ),
        _row("x-live", title="open work"),
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    return {"graph": graph, "tmp": tmp_path}


def _sweep(world) -> None:
    from fno.graph.cli import cmd_archive

    cmd_archive(apply=True, older_than_days=30, roadmap_id=None)


def test_sweep_stamps_archived_at_in_the_store(world):
    _sweep(world)
    assert _archived_at(world["graph"], "x-old"), "the archived row carries a stamp"
    assert _archived_at(world["graph"], "x-live") is None, "the live row stays unstamped"


def test_default_reads_drop_archived_include_archived_keeps_them(world):
    from fno.graph.api import wire_rows

    _sweep(world)
    live = {r["id"] for r in wire_rows(path=world["graph"])}
    everything = {r["id"] for r in wire_rows(path=world["graph"], include_archived=True)}
    assert live == {"x-live"}
    assert everything == {"x-live", "x-old"}


def test_import_skips_an_id_reuse_and_the_live_row_wins(tmp_path, monkeypatch):
    from fno.graph.api import wire_rows
    from fno.graph.store import _client_for, _worker_binary

    graph = tmp_path / "graph.json"
    _seed(graph, _row("x-dup", title="already here"))
    (tmp_path / "graph-archive.json").write_text(
        json.dumps(
            {
                "entries": [
                    _row("x-dup", archived_at="2026-08-01T00:00:00Z", title="old done"),
                    _row("x-fold", archived_at="2026-08-01T00:00:00Z", title="folded in"),
                ]
            }
        ),
        encoding="utf-8",
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    # `version` is the worker's own verb and `node` reads graph.json
    # directly; only an op that opens the db (decisions) reaches the
    # one-shot import and its fold.
    _client_for(graph).request("api", {"op": "decisions"})
    rows = {r["id"]: r for r in wire_rows(path=graph, include_archived=True)}
    assert rows["x-dup"]["title"] == "already here", "the live row wins a reused id"
    assert "x-fold" in rows, "the fold completes around the collision"


def test_import_without_a_file_stamps_nothing_and_folds_later(tmp_path, monkeypatch):
    from fno.graph.api import wire_rows
    from fno.graph import store as store_mod

    graph = tmp_path / "graph.json"
    _seed(graph, _row("x-live", title="here"))
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    store_mod._client_for(graph).request("api", {"op": "decisions"})
    with sqlite3.connect(graph.with_suffix(".db")) as connection:
        stamped = connection.execute(
            "SELECT value FROM graph_meta WHERE key = 'archive_imported_v2'"
        ).fetchone()
    assert stamped is None, "no archive file, no stamp: a restored file must still fold"

    # The restored file arrives AFTER the first open: the exec lane's
    # one-shot child re-runs the import probes per request, so the next
    # open folds it and stamps v2.
    (tmp_path / "graph-archive.json").write_text(
        json.dumps({"entries": [_row("x-late", archived_at="2026-08-01T00:00:00Z")]}),
        encoding="utf-8",
    )
    store_mod._client_for(graph).request("api", {"op": "decisions"})
    rows = {r["id"] for r in wire_rows(path=graph, include_archived=True)}
    assert "x-late" in rows, "the later-restored archive folded on the next spawn"
    with sqlite3.connect(graph.with_suffix(".db")) as connection:
        stamped = connection.execute(
            "SELECT value FROM graph_meta WHERE key = 'archive_imported_v2'"
        ).fetchone()
    assert stamped == ("1",), "the fold stamps v2"


def test_a_v1_poisoned_stamp_voids_and_the_file_folds(tmp_path, monkeypatch):
    from fno.graph.api import wire_rows
    from fno.graph import store as store_mod

    graph = tmp_path / "graph.json"
    _seed(graph, _row("x-live", title="here"))
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    store_mod._client_for(graph).request("api", {"op": "decisions"})
    # A store poisoned by the old behavior: v1 stamped, no v2 key, and the
    # archive file only restored afterwards.
    with sqlite3.connect(graph.with_suffix(".db")) as connection:
        connection.execute(
            "INSERT INTO graph_meta(key, value) VALUES('archive_imported', '1')"
        )
    (tmp_path / "graph-archive.json").write_text(
        json.dumps({"entries": [_row("x-v1", archived_at="2026-08-01T00:00:00Z")]}),
        encoding="utf-8",
    )
    store_mod._client_for(graph).request("api", {"op": "decisions"})
    rows = {r["id"] for r in wire_rows(path=graph, include_archived=True)}
    assert "x-v1" in rows, "the v1 stamp did not block the fold"
    with sqlite3.connect(graph.with_suffix(".db")) as connection:
        stamped = connection.execute(
            "SELECT value FROM graph_meta WHERE key = 'archive_imported_v2'"
        ).fetchone()
    assert stamped == ("1",), "the re-fold stamps v2"


def test_unarchive_clears_the_stamp(world):
    from fno.graph.api import unarchive_node, wire_rows

    _sweep(world)
    payload = unarchive_node("x-old", path=world["graph"])
    assert payload.success
    assert _archived_at(world["graph"], "x-old") is None, "the stamp is gone, not blanked"
    assert "x-old" in {r["id"] for r in wire_rows(path=world["graph"])}
