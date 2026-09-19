"""Shared test reader for the committed event history.

The store cutover made the SQL commit the write boundary, so tests must read
committed rows, never journal bytes: an emitted event leaves no byte trace in
events.jsonl. One helper serves every test that used to slurp the journal:

    from tests._event_rows import event_rows

    rows = event_rows(tmp_path / "events.jsonl")

The read rides the native ``doctor event rows`` verb, which imports any
uncommitted journal bytes before querying, so a fixture seeded before any
store-backed write stays visible. Rows are parsed envelopes in commit order.
When the native binary is unavailable the helper falls back to raw journal
bytes, mirroring the pre-store reader.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Optional


def event_rows(events_path: Path, *, types: Optional[list[str]] = None) -> list[dict[str, Any]]:
    """Committed envelopes for one journal, optionally filtered by type."""
    from fno.events.store_client import native_rows

    events_path = Path(events_path)
    committed = native_rows(events_path)
    if committed is None:
        committed = []
        if events_path.exists():
            committed = events_path.read_text(encoding="utf-8").splitlines()
    rows: list[dict[str, Any]] = []
    for line in committed:
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    if types is not None:
        rows = [r for r in rows if r.get("type") in set(types)]
    return rows
