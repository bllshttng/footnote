"""Unit tests for fno.graph.parity - the in-package JSON/SQLite parity compare.

Ported off scripts/analysis/graph-parity.py: the compare now runs in-process
(no subprocess, no plugin-script lookup) and adds a race guard that reads
graph_meta.exported_version (the sha256 of the JSON bytes the shadow write
just published - graph_store.rs:2262-2268) before trusting a row-by-row
compare.
"""
from __future__ import annotations

import hashlib
import json
import sqlite3
from pathlib import Path

from fno.graph import parity


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
                (row["id"], ordinal, parity._canonical(row)),
            )
        if exported_version is not None:
            connection.execute(
                "INSERT INTO graph_meta VALUES('exported_version', ?)",
                (exported_version,),
            )


def test_compare_clean_passes(tmp_path):
    entries = [{"id": "x-1", "title": "one"}, {"id": "x-2", "title": "two"}]
    graph = tmp_path / "graph.json"
    sha = _write_json(graph, entries)
    db = tmp_path / "graph.db"
    _make_db(db, entries, sha)
    assert parity.compare(graph=graph, db=db) == 0


def test_compare_no_exported_version_still_compares(tmp_path):
    """A db with no exported_version row (never shadow-synced) skips the
    race guard rather than blocking a clean compare."""
    entries = [{"id": "x-1", "title": "one"}]
    graph = tmp_path / "graph.json"
    _write_json(graph, entries)
    db = tmp_path / "graph.db"
    _make_db(db, entries, exported_version=None)
    assert parity.compare(graph=graph, db=db) == 0


def test_compare_content_diverged_names_the_id(tmp_path):
    entries = [{"id": "x-1", "title": "one"}]
    graph = tmp_path / "graph.json"
    sha = _write_json(graph, entries)
    db = tmp_path / "graph.db"
    _make_db(db, [{"id": "x-1", "title": "different"}], sha)
    assert parity.compare(graph=graph, db=db) == 1


def test_compare_missing_and_extra_rows(tmp_path):
    graph = tmp_path / "graph.json"
    sha = _write_json(graph, [{"id": "x-1", "title": "one"}])
    db = tmp_path / "graph.db"
    _make_db(db, [{"id": "x-2", "title": "two"}], sha)
    assert parity.compare(graph=graph, db=db) == 1


def test_compare_race_guard_exits_unmeasured(tmp_path):
    """AC: an exported_version that never agrees with the JSON bytes across
    the retry budget (the shadow write fell behind) reads as UNMEASURED, not
    as a content divergence - it never reaches the row-by-row compare."""
    entries = [{"id": "x-1", "title": "one"}]
    graph = tmp_path / "graph.json"
    _write_json(graph, entries)
    db = tmp_path / "graph.db"
    _make_db(db, [{"id": "x-1", "title": "stale-in-sqlite"}], "sha256:not-the-real-hash")
    assert parity.compare(graph=graph, db=db, retries=2) == 2


def test_compare_malformed_entries_shape_is_unmeasured_not_a_crash(tmp_path):
    """`entries` as a non-list (e.g. a dict) must read UNMEASURED, not raise
    an uncaught TypeError out of compare()."""
    graph = tmp_path / "graph.json"
    graph.write_bytes(b'{"entries": {"oops": 1}}')
    db = tmp_path / "graph.db"
    _make_db(db, [], None)
    assert parity.compare(graph=graph, db=db) == 2


def test_negative_control_passes_on_live_shaped_copies(tmp_path):
    entries = [{"id": "x-1", "title": "one"}, {"id": "x-2", "title": "two"}]
    graph = tmp_path / "graph.json"
    sha = _write_json(graph, entries)
    db = tmp_path / "graph.db"
    _make_db(db, entries, sha)
    assert parity.negative_control(graph=graph, db=db) == 0


def test_compare_duplicate_id_in_json_is_unmeasured(tmp_path):
    """A duplicate id is refused loudly (UNMEASURED), never silently
    collapsed to whichever row happened to load last."""
    graph = tmp_path / "graph.json"
    sha = _write_json(graph, [{"id": "x-1", "title": "a"}, {"id": "x-1", "title": "b"}])
    db = tmp_path / "graph.db"
    _make_db(db, [{"id": "x-1", "title": "a"}], sha)
    assert parity.compare(graph=graph, db=db) == 2


def test_negative_control_prints_pass_with_id(tmp_path, capsys):
    entries = [{"id": "x-1", "title": "one"}]
    graph = tmp_path / "graph.json"
    sha = _write_json(graph, entries)
    db = tmp_path / "graph.db"
    _make_db(db, entries, sha)
    rc = parity.negative_control(graph=graph, db=db)
    assert rc == 0
    captured = capsys.readouterr()
    assert "negative control: PASS x-1" in captured.out
