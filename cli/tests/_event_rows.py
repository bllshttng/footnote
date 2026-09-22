"""Shared test reader for the committed event history.

The store cutover made the SQL commit the write boundary, so tests must read
committed rows, never journal bytes: an emitted event leaves no byte trace in
events.jsonl. One helper serves every test that used to slurp the journal:

    from tests._event_rows import event_rows

    rows = event_rows(tmp_path / "events.jsonl")

The fast path reads the sibling store DIRECTLY over read-only SQL: no process
spawn, no retention side effects - a test asserts state, it never prunes.
A journal holding ANY bytes routes through the native ``doctor event rows``
verb, which imports uncommitted journal bytes before querying: a raw fixture
line written after the last store commit is invisible to direct SQL. With no
store at all the helper answers the raw journal, mirroring the pre-store
reader. Rows are parsed envelopes in commit order.
"""
from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from typing import Any, Optional


def _direct_store_lines(db: Path) -> Optional[list[str]]:
    """Committed envelope lines straight from the store, or None if unreadable."""
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        return [row[0] for row in conn.execute("SELECT line FROM events ORDER BY seq")]
    except sqlite3.DatabaseError:
        return None
    finally:
        conn.close()


def event_rows(events_path: Path, *, types: Optional[list[str]] = None) -> list[dict[str, Any]]:
    """Committed envelopes for one journal, optionally filtered by type."""
    from fno.events.store_client import native_rows, store_db_path

    events_path = Path(events_path)
    committed: Optional[list[str]] = None
    db = store_db_path(events_path)
    # The fast path is only safe when the journal holds no bytes a store
    # commit could have skipped importing: a raw fixture line written after
    # the last store commit stays invisible to a direct SQL read. Journal
    # bytes are rare once writes commit through the store, so tests seeded
    # only through append_event keep the no-spawn path.
    journal_has_bytes = events_path.exists() and events_path.stat().st_size > 0
    if db.exists() and not journal_has_bytes:
        committed = _direct_store_lines(db)
    if committed is None:
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
