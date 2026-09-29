"""The in-memory hold-verdict adapter the src-tree tests share.

The verdict answers from the graph on disk (one fno-agents receipt); tests
that build in-memory graphs persist each call's rows first. Lives under
tests/ so the test-infrastructure lines stay out of the production tally.
"""

from __future__ import annotations

import json as _json
import sqlite3 as _sqlite3

import pytest


@pytest.fixture(autouse=True)
def in_memory_hold_verdict(tmp_path, monkeypatch):
    from fno.graph import ladder

    real = ladder.dispatch_hold_verdict

    def patched(entry, by_id):
        rows = list(by_id.values())
        if isinstance(entry, dict) and entry not in rows:
            rows = rows + [entry]
        complete = {}
        for e in rows:
            if not isinstance(e, dict) or not e.get("id"):
                continue
            row = {"type": "feature", "priority": "p2", "status": "ready", **e}
            row.setdefault("title", str(row.get("id")))
            row.setdefault("slug", str(row.get("id")))
            complete[str(row["id"])] = row
        graph = tmp_path / "verdict-graph.json"
        for stale in (graph, graph.with_suffix(".db")):
            stale.unlink(missing_ok=True)
        path = graph
        path.parent.mkdir(parents=True, exist_ok=True)
        connection = _sqlite3.connect(path.with_suffix(".db"))
        connection.execute(
            "CREATE TABLE IF NOT EXISTS entries ("
            "id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, row TEXT NOT NULL)"
        )
        for ordinal, entry in enumerate(complete.values()):
            connection.execute(
                "INSERT INTO entries(id, ordinal, row) VALUES (?, ?, ?)",
                (entry["id"], ordinal, _json.dumps(entry)),
            )
        connection.commit()
        connection.close()
        monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
        if not isinstance(entry, dict) or not entry.get("id"):
            # A row the graph cannot carry: read as unheld, as before.
            return None
        return real(entry, by_id)

    return patched
