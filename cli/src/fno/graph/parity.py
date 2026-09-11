"""JSON-export to SQLite graph parity compare.

Ships inside the fno package so ``fno doctor lint graph-parity`` runs
in-process on any install, with no subprocess and no script lookup.
"""

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


def _json_sha256(graph: Path) -> str:
    return f"sha256:{hashlib.sha256(graph.read_bytes()).hexdigest()}"


def _exported_version(db: Path) -> "str | None":
    with sqlite3.connect(db) as connection:
        row = connection.execute(
            "SELECT value FROM graph_meta WHERE key = 'exported_version'"
        ).fetchone()
    return row[0] if row else None


def _stable_version(graph: Path, db: Path, retries: int) -> "tuple[str, str | None] | None":
    """Hash graph.json and read graph_meta.exported_version until they agree
    (the sqlite shadow write stamps exported_version with the sha256 of the
    JSON bytes it just wrote - graph_store.rs:2262-2268, graph_sqlite.rs:174-189).
    A mismatch means the shadow write is mid-flight or fell behind; retry up
    to `retries` times before giving up so a real race reads as UNMEASURED
    rather than a false content divergence."""
    json_sha = ""
    exported: "str | None" = None
    for _ in range(retries):
        json_sha = _json_sha256(graph)
        exported = _exported_version(db)
        if exported is None or exported == json_sha:
            return json_sha, exported
    return None


def _json_rows(path: Path) -> tuple[dict[str, str], list[str], list[str]]:
    value = json.loads(path.read_text(encoding="utf-8"))
    entries = value.get("entries") if isinstance(value, dict) else None
    if not isinstance(entries, list):
        raise ValueError("JSON graph root has no entries list")
    rows: dict[str, str] = {}
    malformed: list[str] = []
    order: list[str] = []
    for index, row in enumerate(entries):
        node_id = row.get("id") if isinstance(row, dict) else None
        if not isinstance(node_id, str) or not node_id:
            malformed.append(f"json[{index}]")
            continue
        if node_id in rows:
            malformed.append(f"json duplicate {node_id}")
            continue
        rows[node_id] = _canonical(row)
        order.append(node_id)
    return rows, malformed, order


def _sqlite_rows(path: Path) -> tuple[dict[str, str], list[str], list[str]]:
    rows: dict[str, str] = {}
    malformed: list[str] = []
    order: list[str] = []
    with sqlite3.connect(path) as connection:
        stored = connection.execute(
            "SELECT id, ordinal, row FROM entries ORDER BY ordinal, id"
        ).fetchall()
    for key, ordinal, body in stored:
        try:
            row = json.loads(body)
        except (TypeError, json.JSONDecodeError):
            malformed.append(f"sqlite {key}: invalid JSON")
            continue
        node_id = row.get("id") if isinstance(row, dict) else None
        if not isinstance(key, str) or not key or node_id != key:
            malformed.append(f"sqlite {key}: id mismatch")
            continue
        if key in rows:
            malformed.append(f"sqlite duplicate {key}")
            continue
        if not isinstance(ordinal, int) or ordinal != len(order):
            malformed.append(f"sqlite {key}: ordinal {ordinal!r}, expected {len(order)}")
        rows[key] = _canonical(row)
        order.append(key)
    return rows, malformed, order


def compare(*, graph: "Path | None" = None, db: "Path | None" = None, retries: int = 3) -> int:
    if graph is None:
        from fno import paths

        graph = paths.graph_json()
    graph = Path(graph)
    db = Path(db or graph.with_suffix(".db"))
    try:
        stable = _stable_version(graph, db, retries)
    except (OSError, sqlite3.Error) as exc:
        print(f"graph-parity: UNMEASURED: {exc}")
        return 2
    if stable is None:
        print(f"graph-parity: UNMEASURED: exported_version race after {retries} attempts")
        return 2
    try:
        json_rows, malformed, json_order = _json_rows(graph)
        sqlite_rows, sqlite_malformed, sqlite_order = _sqlite_rows(db)
    except (OSError, sqlite3.Error, ValueError, json.JSONDecodeError) as exc:
        print(f"graph-parity: UNMEASURED: {exc}")
        return 2
    failures = malformed + sqlite_malformed
    if json_order != sqlite_order:
        failures.append("row order diverged")
    failures.extend(f"missing from SQLite: {key}" for key in sorted(json_rows.keys() - sqlite_rows.keys()))
    failures.extend(f"extra in SQLite: {key}" for key in sorted(sqlite_rows.keys() - json_rows.keys()))
    failures.extend(
        f"content diverged: {key}"
        for key in sorted(json_rows.keys() & sqlite_rows.keys())
        if json_rows[key] != sqlite_rows[key]
    )
    if failures:
        for failure in failures:
            print(f"graph-parity: {failure}")
        return 1
    print(f"graph-parity: PASS: compared {len(json_rows)} rows")
    return 0


def negative_control(*, graph: "Path | None" = None, db: "Path | None" = None) -> int:
    """Copy the live graph.json and graph.db, require a clean compare on the
    copies, mutate one copied row's content directly in the SQLite copy (the
    JSON copy and its exported_version stamp stay untouched, so this cannot
    be mistaken for the race the retry loop in `compare` guards against),
    and require the compare to exit 1 naming that row."""
    if graph is None:
        from fno import paths

        graph = paths.graph_json()
    graph = Path(graph)
    db = Path(db or graph.with_suffix(".db"))
    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw)
        copy_graph = root / "graph.json"
        copy_db = root / "graph.db"
        copy_graph.write_bytes(graph.read_bytes())
        with sqlite3.connect(db) as source, sqlite3.connect(copy_db) as target:
            source.backup(target)

        rc = compare(graph=copy_graph, db=copy_db)
        if rc != 0:
            print(f"negative control: FAIL: clean copies did not compare clean (exit {rc})")
            return 1

        with sqlite3.connect(copy_db) as connection:
            row = connection.execute(
                "SELECT id, row FROM entries ORDER BY ordinal, id LIMIT 1"
            ).fetchone()
            if row is None:
                print("negative control: FAIL: no entries to mutate")
                return 1
            target_id, body = row
            mutated = json.loads(body)
            mutated["title"] = f"{mutated.get('title', '')} (negative control mutation)"
            connection.execute(
                "UPDATE entries SET row = ? WHERE id = ?",
                (_canonical(mutated), target_id),
            )

        rc = compare(graph=copy_graph, db=copy_db)
        if rc != 1:
            print(f"negative control: FAIL: mutated copy exited {rc}, expected 1")
            return 1
        print(f"negative control: PASS {target_id}")
        return 0


def self_test() -> int:
    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw)
        graph, db = root / "graph.json", root / "graph.db"
        graph.write_text('{"entries":[{"id":"x-test","title":"same"}]}')
        with sqlite3.connect(db) as connection:
            connection.execute("CREATE TABLE entries(id TEXT PRIMARY KEY, ordinal INTEGER, row TEXT)")
            connection.execute("CREATE TABLE graph_meta(key TEXT PRIMARY KEY, value TEXT)")
            connection.execute("INSERT INTO entries VALUES(?, ?, ?)", ("x-test", 0, '{"id":"x-test","title":"same"}'))
        if compare(graph=graph, db=db) != 0:
            return 1
        with sqlite3.connect(db) as connection:
            connection.execute(
                "UPDATE entries SET row = ?",
                ('{"id":"x-test","title":"different"}',),
            )
        if compare(graph=graph, db=db) != 1:
            return 1
    print("graph-parity self-test: PASS (clean and divergence controls fired)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--graph", type=Path)
    parser.add_argument("--db", type=Path)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--negative-control", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if args.negative_control:
        return negative_control(graph=args.graph, db=args.db)
    return compare(graph=args.graph, db=args.db)


if __name__ == "__main__":
    raise SystemExit(main())
