"""Unit tests for fno.graph.parity - the thin client over the keeper's
`parity` op (the compare itself lives in backlog::parity, Rust side, and
rides the typed model). Fixtures seed graph.json plus the wave-1 blob
shape, so every scenario also exercises the import-on-open path: a fresh
keeper opens the blob db, imports into the relational tables, and compares
the export against the JSON.
"""

from __future__ import annotations

import hashlib
import json
import sqlite3
from pathlib import Path

import pytest

from fno.graph import parity

FULL = {
    "type": "feature",
    "status": "idea",
    "priority": "p2",
    "domain": "code",
    "slug": "n",
    "created_at": "2026-09-11T00:00:00+00:00",
}


def _row(node_id, **overrides):
    # Identity fields after FULL so each row keeps its own slug: the nodes
    # table enforces slug UNIQUE.
    row = {**FULL, "id": node_id, "slug": node_id, "title": node_id, **overrides}
    return row


def _write_json(path: Path, entries: list[dict]) -> str:
    data = json.dumps({"entries": entries}).encode("utf-8")
    path.write_bytes(data)
    return f"sha256:{hashlib.sha256(data).hexdigest()}"


def _make_db(path: Path, entries: list[dict], exported_version: "str | None") -> None:
    with sqlite3.connect(path) as connection:
        connection.execute(
            "CREATE TABLE entries(id TEXT PRIMARY KEY, ordinal INTEGER, row TEXT)"
        )
        connection.execute(
            "CREATE TABLE graph_meta(key TEXT PRIMARY KEY, value TEXT)"
        )
        for ordinal, row in enumerate(entries):
            connection.execute(
                "INSERT INTO entries VALUES(?, ?, ?)",
                (row["id"], ordinal, json.dumps(row, sort_keys=True)),
            )
    if exported_version is not None:
        with sqlite3.connect(path) as connection:
            connection.execute(
                "INSERT INTO graph_meta VALUES('exported_version', ?)",
                (exported_version,),
            )


@pytest.fixture(autouse=True)
def _worker(monkeypatch):
    """Point the worker binary at the repo's debug build so the spawned
    keeper carries the keeper-op surface under test."""
    from pathlib import Path as _P

    worker = _P(__file__).parents[3] / "crates" / "fno-agents" / "target" / "debug" / "fno-agents-worker"
    if worker.exists():
        monkeypatch.setenv("FNO_AGENTS_WORKER", str(worker))


def test_compare_clean_passes(tmp_path):
    entries = [_row("x-1", title="one"), _row("x-2", title="two")]
    graph = tmp_path / "graph.json"
    _write_json(graph, entries)
    db = tmp_path / "graph.db"
    _make_db(db, entries, exported_version=None)
    assert parity.compare(graph=graph, db=db) == 0


def test_compare_content_diverged_names_the_id(tmp_path):
    entries = [_row("x-1", title="one")]
    graph = tmp_path / "graph.json"
    _write_json(graph, entries)
    db = tmp_path / "graph.db"
    _make_db(db, [_row("x-1", title="different")], exported_version=None)
    assert parity.compare(graph=graph, db=db) == 1


def test_compare_missing_and_extra_rows(tmp_path):
    graph = tmp_path / "graph.json"
    _write_json(graph, [_row("x-1", title="one")])
    db = tmp_path / "graph.db"
    _make_db(db, [_row("x-2", title="two")], exported_version=None)
    assert parity.compare(graph=graph, db=db) == 1


def test_compare_malformed_entries_shape_is_unmeasured_not_a_crash(tmp_path):
    """`entries` as a non-list (e.g. a dict) must read UNMEASURED, not raise
    an uncaught TypeError out of compare()."""
    graph = tmp_path / "graph.json"
    graph.write_bytes(b'{"entries": {"oops": 1}}')
    db = tmp_path / "graph.db"
    _make_db(db, [], exported_version=None)
    assert parity.compare(graph=graph, db=db) == 2


def test_compare_duplicate_id_in_json_is_unmeasured(tmp_path):
    """A duplicate id is refused loudly (UNMEASURED), never silently
    collapsed to whichever row happened to load last."""
    graph = tmp_path / "graph.json"
    _write_json(graph, [_row("x-1", title="a"), _row("x-1", title="b")])
    db = tmp_path / "graph.db"
    _make_db(db, [_row("x-1", title="a")], exported_version=None)
    assert parity.compare(graph=graph, db=db) == 2


def test_compare_no_worker_binary_is_unmeasured(tmp_path, monkeypatch):
    """A store that cannot be reached reads UNMEASURED, never divergence -
    the same contract the old race-guard scenario carried."""
    graph = tmp_path / "graph.json"
    _write_json(graph, [_row("x-1", title="one")])
    monkeypatch.setenv("FNO_AGENTS_WORKER", "/nonexistent/worker")
    assert parity.compare(graph=graph, db=tmp_path / "graph.db") == 2


def test_negative_control_passes_on_live_shaped_copies(tmp_path):
    entries = [_row("x-1", title="one"), _row("x-2", title="two")]
    graph = tmp_path / "graph.json"
    _write_json(graph, entries)
    db = tmp_path / "graph.db"
    _make_db(db, entries, exported_version=None)
    assert parity.negative_control(graph=graph, db=db) == 0


def test_negative_control_prints_pass_with_id(tmp_path, capsys):
    entries = [_row("x-1", title="one")]
    graph = tmp_path / "graph.json"
    _write_json(graph, entries)
    db = tmp_path / "graph.db"
    _make_db(db, entries, exported_version=None)
    rc = parity.negative_control(graph=graph, db=db)
    assert rc == 0
    captured = capsys.readouterr()
    assert "negative control: PASS x-1" in captured.out
