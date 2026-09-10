"""Temporary JSON-export to SQLite graph parity gate."""

from __future__ import annotations

import argparse
import json
import sqlite3
from pathlib import Path
from typing import Any


def _canonical(row: dict[str, Any]) -> str:
    return json.dumps(row, ensure_ascii=True, sort_keys=True, separators=(",", ":"))


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


def compare(*, graph: Path | None = None, db: Path | None = None) -> int:
    from fno import paths

    graph = Path(graph or paths.graph_json())
    db = Path(db or graph.with_suffix(".db"))
    try:
        json_rows, malformed, json_order = _json_rows(graph)
        sqlite_rows, sqlite_malformed, sqlite_order = _sqlite_rows(db)
    except (OSError, sqlite3.Error, ValueError, json.JSONDecodeError) as exc:
        print(f"graph-parity: UNMEASURED: {exc}")
        return 2
    failures = malformed + sqlite_malformed
    if json_order != sqlite_order:
        failures.append("row order diverged")
    failures.extend(f"missing from SQLite: {node_id}" for node_id in sorted(json_rows.keys() - sqlite_rows.keys()))
    failures.extend(f"extra in SQLite: {node_id}" for node_id in sorted(sqlite_rows.keys() - json_rows.keys()))
    failures.extend(
        f"content diverged: {node_id}"
        for node_id in sorted(json_rows.keys() & sqlite_rows.keys())
        if json_rows[node_id] != sqlite_rows[node_id]
    )
    if failures:
        for failure in failures:
            print(f"graph-parity: {failure}")
        return 1
    print(f"graph-parity: PASS: compared {len(json_rows)} rows")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--graph", type=Path)
    parser.add_argument("--db", type=Path)
    args = parser.parse_args()
    return compare(graph=args.graph, db=args.db)


if __name__ == "__main__":
    raise SystemExit(main())
