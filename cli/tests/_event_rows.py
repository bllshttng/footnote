"""Shared test reader for the committed event history.

The store cutover made the SQL commit the write boundary, so tests must read
committed rows, never journal bytes: an emitted event leaves no byte trace in
events.jsonl. One helper serves every test that used to slurp the journal:

    from tests._event_rows import event_rows

    rows = event_rows(tmp_path / "events.jsonl")

Semantics mirror the Rust readers (``event_lines``): a store that does not
exist yet falls back to raw journal bytes, so a fixture seeded before any
store-backed write stays visible. Rows are parsed envelopes in commit order.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Optional


def event_rows(events_path: Path, *, types: Optional[list[str]] = None) -> list[dict[str, Any]]:
    """Committed envelopes for one journal, optionally filtered by type."""
    from fno.events.store_client import read_committed_lines, store_db_path

    events_path = Path(events_path)
    if not store_db_path(events_path).exists():
        rows: list[dict[str, Any]] = []
        if events_path.exists():
            for raw in events_path.read_text(encoding="utf-8").splitlines():
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    rows.append(json.loads(raw))
                except json.JSONDecodeError:
                    continue
        else:
            rows = []
    else:
        rows = []
        for line in read_committed_lines(events_path):
            if not line.strip():
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    if types is not None:
        rows = [r for r in rows if r.get("type") in set(types)]
    return rows
