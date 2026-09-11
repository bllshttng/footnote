"""JSON/SQLite graph parity compare. Runs in-process: no subprocess, no script lookup."""

from __future__ import annotations

import argparse
import hashlib
import json
import sqlite3
import tempfile
from pathlib import Path
from typing import Any

def _canonical(row: dict[str, Any]) -> str:
    return json.dumps(row, ensure_ascii=True, sort_keys=True, separators=(",", ":"))

def _rows(items: "list[tuple[str, dict]]", label: str) -> dict[str, str]:
    """Refuse a duplicate id rather than silently keeping the last one."""
    ids = [node_id for node_id, _ in items]
    if len(ids) != len(set(ids)):
        raise ValueError(f"duplicate id in {label}")
    return {node_id: _canonical(row) for node_id, row in items}

def _resolve(graph: "Path | None", db: "Path | None") -> tuple[Path, Path]:
    if graph is None:
        from fno import paths
        graph = paths.graph_json()
    graph = Path(graph)
    return graph, Path(db or graph.with_suffix(".db"))

def _stable_bytes(graph: Path, db: Path, retries: int) -> "bytes | None":
    """Retry until graph.json's sha256 matches the shadow write's exported_version stamp."""
    for _ in range(retries):
        data = graph.read_bytes()
        with sqlite3.connect(db) as c:
            row = c.execute("SELECT value FROM graph_meta WHERE key = 'exported_version'").fetchone()
        exported = row[0] if row else None
        if exported is None or exported == f"sha256:{hashlib.sha256(data).hexdigest()}":
            return data
    return None

def compare(*, graph: "Path | None" = None, db: "Path | None" = None, retries: int = 3) -> int:
    graph, db = _resolve(graph, db)
    try:
        graph_bytes = _stable_bytes(graph, db, retries)
        if graph_bytes is None:
            print(f"graph-parity: UNMEASURED: exported_version race after {retries} attempts")
            return 2
        json_rows = _rows([(r["id"], r) for r in json.loads(graph_bytes)["entries"]], "graph.json")
        with sqlite3.connect(db) as c:
            stored = c.execute("SELECT id, row FROM entries ORDER BY ordinal, id").fetchall()
        sqlite_rows = _rows([(k, json.loads(b)) for k, b in stored], "SQLite entries")
    except (OSError, sqlite3.Error, KeyError, ValueError, json.JSONDecodeError) as exc:
        print(f"graph-parity: UNMEASURED: {exc}")
        return 2
    failures = [f"missing from SQLite: {k}" for k in sorted(json_rows.keys() - sqlite_rows.keys())]
    failures += [f"extra in SQLite: {k}" for k in sorted(sqlite_rows.keys() - json_rows.keys())]
    failures += [f"content diverged: {k}" for k in sorted(json_rows.keys() & sqlite_rows.keys())
                 if json_rows[k] != sqlite_rows[k]]
    if failures:
        for failure in failures:
            print(f"graph-parity: {failure}")
        return 1
    print(f"graph-parity: PASS: compared {len(json_rows)} rows")
    return 0

def negative_control(*, graph: "Path | None" = None, db: "Path | None" = None) -> int:
    """Copy, require a clean compare, mutate one copied SQLite row, require exit 1 naming it."""
    graph, db = _resolve(graph, db)
    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw)
        copy_graph, copy_db = root / "graph.json", root / "graph.db"
        copy_graph.write_bytes(graph.read_bytes())
        with sqlite3.connect(db) as source, sqlite3.connect(copy_db) as target:
            source.backup(target)
        if compare(graph=copy_graph, db=copy_db) != 0:
            print("negative control: FAIL: clean copies did not compare clean")
            return 1
        with sqlite3.connect(copy_db) as c:
            row = c.execute("SELECT id, row FROM entries ORDER BY ordinal, id LIMIT 1").fetchone()
            if row is None:
                print("negative control: FAIL: no entries to mutate")
                return 1
            target_id, body = row
            mutated = json.loads(body)
            mutated["title"] = f"{mutated.get('title', '')} (negative control mutation)"
            c.execute("UPDATE entries SET row = ? WHERE id = ?", (_canonical(mutated), target_id))
        if compare(graph=copy_graph, db=copy_db) != 1:
            print("negative control: FAIL: mutated copy did not diverge")
            return 1
        print(f"negative control: PASS {target_id}")
        return 0

def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--graph", type=Path)
    parser.add_argument("--db", type=Path)
    parser.add_argument("--negative-control", action="store_true")
    args = parser.parse_args()
    if args.negative_control:
        return negative_control(graph=args.graph, db=args.db)
    return compare(graph=args.graph, db=args.db)

if __name__ == "__main__":
    raise SystemExit(main())
