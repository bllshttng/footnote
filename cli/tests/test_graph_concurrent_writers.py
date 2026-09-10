"""Concurrent writer regression coverage for row-scoped graph commits."""
from __future__ import annotations

import json
import multiprocessing as mp
import queue
from pathlib import Path

import pytest

from fno.rust_binary import find_dev_binary

pytestmark = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary is required",
)


def _write_notes(
    graph: str,
    node_id: str,
    prefix: str,
    barrier,
    errors,
    invocations,
) -> None:
    from fno.graph.store import locked_mutate_graph

    invoked = 0
    for index in range(50):
        marker = f"{prefix}-{index}"
        synchronized = False

        def mutate(entries):
            nonlocal invoked, synchronized
            invoked += 1
            if not synchronized:
                barrier.wait(timeout=30)
                synchronized = True
            row = next(entry for entry in entries if entry.get("id") == node_id)
            row.setdefault("progress_notes", []).append(
                {"ts": f"2026-09-09T00:00:{index:02d}Z", "text": marker}
            )
            return entries

        try:
            locked_mutate_graph(Path(graph), mutate)
        except BaseException as exc:
            errors.put(f"{prefix}-{index}: {type(exc).__name__}: {exc}")
            return
    invocations.put((prefix, invoked))


def _run_pair(
    tmp_path: Path, left: str, right: str
) -> tuple[dict[str, list[str]], dict[str, int]]:
    from fno.graph import store

    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "x-left", "title": "left", "progress_notes": []},
                    {"id": "x-right", "title": "right", "progress_notes": []},
                ]
            }
        )
        + "\n"
    )
    store._client_for(graph).request("read", {"strict": False})
    context = mp.get_context("fork")
    barrier = context.Barrier(2)
    errors = context.Queue()
    invocations = context.Queue()
    workers = [
        context.Process(
            target=_write_notes,
            args=(str(graph), left, "a", barrier, errors, invocations),
        ),
        context.Process(
            target=_write_notes,
            args=(str(graph), right, "b", barrier, errors, invocations),
        ),
    ]
    for worker in workers:
        worker.start()
    for worker in workers:
        worker.join(timeout=120)
        assert worker.exitcode == 0, f"writer exit={worker.exitcode}"
    reported = []
    while True:
        try:
            reported.append(errors.get_nowait())
        except queue.Empty:
            break
    assert reported == [], reported
    rows = json.loads(graph.read_text())["entries"]
    notes = {
        row["id"]: [note["text"] for note in row.get("progress_notes", [])]
        for row in rows
    }
    attempts = dict(invocations.get(timeout=5) for _ in workers)
    return notes, attempts


def test_disjoint_writers_land_one_hundred_notes_without_conflicts(tmp_path: Path) -> None:
    notes, attempts = _run_pair(tmp_path, "x-left", "x-right")
    assert len(notes["x-left"]) == 50
    assert len(notes["x-right"]) == 50
    assert len(set(notes["x-left"] + notes["x-right"])) == 100
    assert attempts == {"a": 50, "b": 50}


def test_same_row_writers_land_exactly_one_hundred_unique_notes(tmp_path: Path) -> None:
    rows, _attempts = _run_pair(tmp_path, "x-left", "x-left")
    notes = rows["x-left"]
    assert len(notes) == 100
    assert len(set(notes)) == 100
