"""Seed fixture rows into the SQLite store, bypassing intake policy."""

from __future__ import annotations

import json
import sqlite3
import sys
from pathlib import Path
from typing import Iterable

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))


def seed_graph(path: Path, entries: Iterable[dict] | str | bytes | dict) -> list[dict]:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(entries, bytes):
        entries = entries.decode("utf-8")
    if isinstance(entries, str):
        entries = json.loads(entries)
    if isinstance(entries, dict):
        entries = entries.get("entries")
    if isinstance(entries, Iterable) and not isinstance(entries, (str, bytes, dict, list)):
        entries = list(entries)
    if not isinstance(entries, list):
        raise TypeError("graph seed must be an entries list or an entries document")
    rows = [dict(entry) for entry in entries]

    store_path = path.with_suffix(".db")
    store_path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(store_path)
    try:
        tables = {
            row[0]
            for row in connection.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table'"
            )
        }
        stored = 0
        if "nodes" in tables:
            stored += connection.execute("SELECT COUNT(*) FROM nodes").fetchone()[0]
        if "nodes_raw" in tables:
            stored += connection.execute("SELECT COUNT(*) FROM nodes_raw").fetchone()[0]

        if stored == 0:
            connection.execute(
                "CREATE TABLE IF NOT EXISTS entries ("
                "id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, row TEXT NOT NULL)"
            )
            ordinal = connection.execute(
                "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM entries"
            ).fetchone()[0]
            for entry in rows:
                node_id = entry.get("id")
                if not isinstance(node_id, str):
                    continue
                connection.execute(
                    "INSERT INTO entries(id, ordinal, row) VALUES (?, ?, ?)",
                    (node_id, ordinal, json.dumps(entry)),
                )
                ordinal += 1
            connection.commit()
            connection.close()
            from fno.graph.store import read_graph_strict

            read_graph_strict(path)
            return rows
    finally:
        if connection:
            connection.close()

    from fno.graph.store import _client_for, _commit_rows, _read_snapshot

    client = _client_for(path)
    version, current, digests = _read_snapshot(client)
    _commit_rows(client, version, digests, current, rows, {}, 1)
    return rows


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: graph_seed.py <graph.json anchor>")
    seed_graph(Path(sys.argv[1]), sys.stdin.buffer.read())
