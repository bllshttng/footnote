"""Unit tests for ``fno doctor graph backend``. SQLite is the only store,
so the flip verb stamps the backend meta and refuses every other name; the
tree checks (reader census, writer ratchet, table ownership) run in CI.
Here the verbs run against a real keeper on a temp graph, so the
operator's live graph and config are never touched."""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest
import typer

from fno import doctor_graph


def _row(node_id: str, **overrides):
    return {
        "type": "feature",
        "status": "idea",
        "priority": "p2",
        "domain": "code",
        "created_at": "2026-09-11T00:00:00+00:00",
        "id": node_id,
        "slug": node_id,
        "title": node_id,
        "tags": [],
        **overrides,
    }


def _meta(graph: Path, key: str):
    try:
        with sqlite3.connect(graph.with_suffix(".db")) as connection:
            row = connection.execute(
                "SELECT value FROM graph_meta WHERE key = ?", (key,)
            ).fetchone()
    except sqlite3.OperationalError:
        # An untouched db carries no tables yet: the key reads unset.
        return None
    return row[0] if row else None


@pytest.fixture
def world(tmp_path, monkeypatch):
    """A temp machine: graph and state root. Skips where no keeper binary
    can spawn."""
    from fno.graph.store import _worker_binary

    if _worker_binary() is None:
        pytest.skip("no fno-agents-worker binary; build with `cargo build -p fno-agents`")
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps({"entries": [_row("x-1", title="one"), _row("x-2", title="two")]}),
        encoding="utf-8",
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    return {"graph": graph, "tmp": tmp_path}


def test_happy_flip_to_sqlite_stamps_backend(world):
    doctor_graph._flip("sqlite")
    assert _meta(world["graph"], "backend") == "sqlite"
    assert _meta(world["graph"], "backend_since_ms") is not None


def test_flip_refuses_the_deleted_json_backend(world):
    with pytest.raises(typer.Exit):
        doctor_graph._flip("json")
    assert _meta(world["graph"], "backend") is None


def test_flip_is_idempotent_and_keeps_the_since_stamp(world):
    doctor_graph._flip("sqlite")
    since_first = _meta(world["graph"], "backend_since_ms")
    doctor_graph._flip("sqlite")
    assert _meta(world["graph"], "backend_since_ms") == since_first


def test_the_store_is_the_only_writer_and_the_export_stays_frozen(world):
    from fno.graph.store import _client_for

    doctor_graph._flip("sqlite")
    client = _client_for(world["graph"])
    client.request(
        "op",
        {
            "name": "append_progress_note",
            "params": {
                "node_id": "x-1",
                "note": {"ts": "2026-09-14T12:00:00Z", "text": "flip probe"},
            },
        },
    )
    body = world["graph"].read_text(encoding="utf-8")
    assert "flip probe" not in body, "no write path touches graph.json; only export --now does"


def test_status_prints_the_status_line(world, capsys):
    doctor_graph._flip("sqlite")
    capsys.readouterr()
    doctor_graph.graph_backend("status")
    out = capsys.readouterr().out
    assert out.startswith("backend=sqlite since=")
    assert " days=0 keepers=" in out
