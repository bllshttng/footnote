"""graph_tx_conflict: the write path names its retries and backs off.

Measured 2026-09-09: 1156 journal events during the write livelock and not
one named a graph write, a transaction, a conflict or a retry. The
`except _Conflict: continue` path was invisible while it re-shipped the whole
graph in lockstep. Every assertion here is on a positive marker: a parsed
envelope event whose `type` is `graph_tx_conflict`.
"""
from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from fno.events import validate as validate_event
from fno.graph import store


class _FakeClient:
    """begin returns a stable snapshot; the first `conflicts` commits raise."""

    def __init__(self, conflicts: int, entries: list[dict[str, Any]]):
        self.conflicts = conflicts
        self.entries = entries
        self.commits = 0

    def request(self, verb: str, payload: dict[str, Any]) -> dict[str, Any]:
        if verb == "begin":
            return {"version": "v1", "entries": self.entries}
        if verb == "commit":
            self.commits += 1
            if self.commits <= self.conflicts:
                raise store._Conflict()
            return {
                "dropped": 0,
                "backup": None,
                "closure_releases": [],
                "entries": payload["entries"],
            }
        raise AssertionError(f"unexpected verb {verb}")


@pytest.fixture
def journal(monkeypatch: pytest.MonkeyPatch) -> list[dict[str, Any]]:
    """Capture envelope events, running the REAL schema validator on each.

    The real append_event is bypassed (hermetic: no journal file), but its
    validate() step runs, so a schema-drifted row fails here and not only in
    the parity corpus CI check.
    """
    rows: list[dict[str, Any]] = []

    def _capture(event: dict[str, Any], events_path: Any = None) -> None:
        validate_event(event)
        rows.append(event)

    import fno.events

    monkeypatch.setattr(fno.events, "append_event", _capture)
    return rows


@pytest.fixture
def tx(monkeypatch: pytest.MonkeyPatch):
    """Wire a scratch graph to a fake client and record the backoff sleeps."""
    sleeps: list[float] = []
    monkeypatch.setattr(store.time, "sleep", sleeps.append)

    def _install(client: _FakeClient, graph: Path) -> None:
        monkeypatch.setattr(store, "_client_for", lambda path: client)
        monkeypatch.setattr(
            store, "_finish_mutation", lambda path, outcome: outcome["entries"]
        )

    return sleeps, _install


def _conflicts(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [r for r in rows if r.get("type") == "graph_tx_conflict"]


def _graph(tmp_path: Path) -> Path:
    g = tmp_path / "graph.json"
    g.write_text("[]")
    return g


def test_two_conflicts_emit_two_events_then_commit(
    tmp_path: Path, journal: list, tx
) -> None:
    sleeps, install = tx
    g = _graph(tmp_path)
    install(_FakeClient(conflicts=2, entries=[]), g)

    committed = store.locked_mutate_graph(g, lambda entries: entries)

    assert committed == []
    rows = _conflicts(journal)
    assert len(rows) == 2, rows
    assert [r["data"]["attempt"] for r in rows] == [1, 2]
    assert all(r["data"]["exhausted"] is False for r in rows)
    assert [r["data"]["attempts_max"] for r in rows] == [5, 5]
    assert all(r["data"]["graph_path"] == str(g) for r in rows)
    assert all(r["source"] == "python" for r in rows)
    assert len(sleeps) == 2


def test_exhaustion_emits_exhausted_then_raises(
    tmp_path: Path, journal: list, tx
) -> None:
    sleeps, install = tx
    g = _graph(tmp_path)
    install(_FakeClient(conflicts=5, entries=[]), g)

    with pytest.raises(RuntimeError, match="graph mutated under us 5 times"):
        store.locked_mutate_graph(g, lambda entries: entries)

    rows = _conflicts(journal)
    assert len(rows) == 5, rows
    assert rows[-1]["data"]["exhausted"] is True
    assert [r["data"]["attempt"] for r in rows] == [1, 2, 3, 4, 5]
    # Backoff ran before every retry: four sleeps, each within its
    # full-jitter bound, so wall time is at least their sum.
    assert len(sleeps) == 4
    for i, slept in enumerate(sleeps):
        bound = min(store._TX_BACKOFF_CAP_S, store._TX_BACKOFF_BASE_S * 2**i)
        assert 0 <= slept <= bound, (i, slept, bound)


def test_an_unwritable_journal_never_changes_the_outcome(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    def _boom(event: dict[str, Any], events_path: Any = None) -> None:
        raise OSError("read-only file system")

    import fno.events

    monkeypatch.setattr(fno.events, "append_event", _boom)
    monkeypatch.setattr(store.time, "sleep", lambda _s: None)
    g = _graph(tmp_path)
    monkeypatch.setattr(
        store, "_client_for", lambda path: _FakeClient(conflicts=0, entries=[])
    )
    monkeypatch.setattr(
        store, "_finish_mutation", lambda path, outcome: outcome["entries"]
    )

    assert store.locked_mutate_graph(g, lambda entries: entries) == []
