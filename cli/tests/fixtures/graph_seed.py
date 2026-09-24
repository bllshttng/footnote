"""Seed a test graph through the keeper so graph.db is the fixture source."""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Iterable

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))


def seed_graph(path: Path, entries: Iterable[dict] | str | bytes | dict) -> list[dict]:
    from fno.graph.store import _client_for, _commit_rows, _read_snapshot

    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(entries, bytes):
        entries = entries.decode("utf-8")
    if isinstance(entries, str):
        entries = json.loads(entries)
    if isinstance(entries, dict):
        entries = entries.get("entries")
    if not isinstance(entries, list):
        raise TypeError("graph seed must be an entries list or an entries document")
    rows = [dict(entry) for entry in entries]
    client = _client_for(path)
    version, current, digests = _read_snapshot(client)
    _commit_rows(client, version, digests, current, rows, {}, 1)
    return rows


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: graph_seed.py <graph.json anchor>")
    seed_graph(Path(sys.argv[1]), sys.stdin.buffer.read())
