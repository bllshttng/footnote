"""Join dispatches and refuses loudly: one event per outcome, brief inputs named.

Before x-32db task 4.1, ``fno backlog join`` was the one verb that produced
multi-worker dispatch and recorded nothing about it - the only join-named rows
in the journal were write-guard decisions. A join's width, bands and worker
names lived only in the caller's stdout.

Assertions sit on POSITIVE markers: a parsed JSON line whose ``kind`` is
``join_dispatched`` / ``join_refused``. Never a line count alone, never the
absence of an error.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from fno.agents import events as agent_events
from fno.backlog.advance import JoinRefuse, join_node
from tests.unit.test_backlog_join import BANDED_PLAN, PARALLEL_PLAN, SEQUENTIAL_PLAN, _wire


@pytest.fixture
def journal(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Redirect the agents events journal into the test's own tmp dir.

    Belt and braces with the conftest per-module pin: the fixture also sets
    FNO_EVENTS_PATH so any non-patched emitter in this module lands here too.
    """
    target = tmp_path / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(target))
    real_emit = agent_events.emit
    monkeypatch.setattr(
        agent_events,
        "emit",
        lambda kind, **data: real_emit(kind, path=target, **data),
    )
    return target


def _rows(journal: Path) -> list[dict[str, Any]]:
    if not journal.exists():
        return []
    return [
        json.loads(line)
        for line in journal.read_text().splitlines()
        if line.strip()
    ]


def test_join_emits_one_dispatched_event_carrying_the_brief_inputs(
    journal: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC13-HP: width 3 plan, --workers 3, two joiners spawned, one event.

    The positive marker is the parsed ``spawned`` list of length 2, not a
    line count.
    """
    calls = _wire(monkeypatch, tmp_path, PARALLEL_PLAN)
    receipt = join_node("x-8d1d", 3)

    assert len(calls) == 2, calls
    rows = [r for r in _rows(journal) if r.get("kind") == "join_dispatched"]
    assert len(rows) == 1, rows
    row = rows[0]
    assert row["node"] == "x-8d1d"
    assert row["width"] == receipt["width"] == 3
    assert row["requested"] == 3
    assert row["spawned"] == receipt["spawned"]
    assert len(row["spawned"]) == 2
    assert row["lead"] == receipt["lead"] == "j-x-8d1d-1"
    assert row["bands"] == {name: "" for name in row["spawned"]}


def test_banded_join_event_names_each_workers_band(
    journal: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _wire(monkeypatch, tmp_path, BANDED_PLAN)
    receipt = join_node("x-8d1d", 4)

    rows = [r for r in _rows(journal) if r.get("kind") == "join_dispatched"]
    assert len(rows) == 1, rows
    assert rows[0]["bands"] == {
        name: receipt["lanes"][name]["band"] for name in receipt["spawned"]
    }
    assert set(rows[0]["bands"].values()) == {"high", "medium", "low"}


def test_refused_join_emits_exactly_one_refused_event(
    journal: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC13-HP refused arm: a width-1 plan emits join_refused naming the reason."""
    _wire(monkeypatch, tmp_path, SEQUENTIAL_PLAN)

    with pytest.raises(JoinRefuse) as excinfo:
        join_node("x-8d1d", 3)

    assert excinfo.value.code == 3
    rows = _rows(journal)
    refused = [r for r in rows if r.get("kind") == "join_refused"]
    dispatched = [r for r in rows if r.get("kind") == "join_dispatched"]
    assert len(refused) == 1, rows
    assert refused[0]["node"] == "x-8d1d"
    assert refused[0]["code"] == 3
    assert "width" in refused[0]["reason"]
    assert dispatched == [], rows
