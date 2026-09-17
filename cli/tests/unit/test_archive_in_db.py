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
    graph.write_text(
        json.dumps({"entries": list(rows)}),
        encoding="utf-8",
    )


@pytest.fixture
def world(tmp_path, monkeypatch):
    """A temp machine: graph + state root. Skips where no keeper binary
    can spawn."""
    from fno.graph.store import _worker_binary

    if _worker_binary() is None:
        pytest.skip("no fno-agents-worker binary; build with `cargo build -p fno-agents`")
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
    from fno import doctor_graph

    monkeypatch.setattr(doctor_graph, "_gate_gaps", lambda client: [])
    monkeypatch.setattr(doctor_graph, "_keeper_gaps", lambda client: [])
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


def test_only_export_now_writes_the_archive_file(world):
    from fno import doctor_graph
    from fno.graph.store import read_archive_entries

    archive = world["graph"].parent / "graph-archive.json"
    _sweep(world)
    assert not archive.exists(), "the sweep writes rows, never the advisory file"
    doctor_graph._flip("sqlite")  # export is a sqlite-backend read
    doctor_graph.export_graph(now=True)
    assert archive.exists()
    folded = read_archive_entries(path=world["graph"])
    assert [e["id"] for e in folded] == ["x-old"], "export rebuilds it from residents"


def test_import_refuses_when_an_id_already_lives_in_nodes(tmp_path, monkeypatch):
    from fno.graph.store import _client_for, _worker_binary

    if _worker_binary() is None:
        pytest.skip("no fno-agents-worker binary; build with `cargo build -p fno-agents`")
    graph = tmp_path / "graph.json"
    _seed(graph, _row("x-dup", title="already here"))
    (tmp_path / "graph-archive.json").write_text(
        json.dumps({"entries": [_row("x-dup", archived_at="2026-08-01T00:00:00Z")]}),
        encoding="utf-8",
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    # `version` is the worker's own verb and `node` reads graph.json
    # directly; only an op that opens the db (decisions) reaches the
    # one-shot import and its refusal.
    with pytest.raises(RuntimeError, match="already exists in nodes"):
        _client_for(graph).request("api", {"op": "decisions"})


def test_unarchive_clears_the_stamp(world):
    from fno.graph.api import unarchive_node, wire_rows

    _sweep(world)
    payload = unarchive_node("x-old", path=world["graph"])
    assert payload.success
    assert _archived_at(world["graph"], "x-old") is None, "the stamp is gone, not blanked"
    assert "x-old" in {r["id"] for r in wire_rows(path=world["graph"])}
