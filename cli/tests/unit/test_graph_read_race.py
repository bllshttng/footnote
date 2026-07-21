"""US6 / AC3-FR: the reported symptom is covered by a regression test.

The incident: `fno backlog get --strict x-d157` returned a confident
"No node matching" while that row was present, under concurrent write load. This
drives real writers through locked_mutate_graph against real bytes on disk and
reads a known-present node back through the ACTUAL resolution path
(read_graph_strict + resolve_node -- exactly what cmd_get calls, no mocked or
stubbed reader). It asserts zero false negatives, and it fails if the corruption
swallow is reintroduced on the resolution path.
"""
from __future__ import annotations

import json
import threading
from pathlib import Path

import pytest

from fno.graph.fuzzy import resolve_node
from fno.graph.store import (
    GraphUnreadableError,
    locked_mutate_graph,
    read_graph,
    read_graph_strict,
)


@pytest.fixture
def scratch(tmp_path, monkeypatch):
    import fno.graph.store as gs

    # A scratch lock so the harness never takes the real /tmp graph lock and
    # blocks live writers (GRAPH_LOCK_FILE is a hardcoded global path).
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    g = tmp_path / "graph.json"

    def _seed(entries):
        entries.append({"id": "x-keep", "title": "present throughout",
                        "status": "ready", "project": "fno", "domain": "code",
                        "slug": "present-throughout"})
        return entries

    locked_mutate_graph(g, _seed)
    return g


def test_ac3fr_no_false_negative_under_concurrent_writes(scratch):
    misses: list[str] = []
    read_failures: list[Exception] = []
    stop = threading.Event()

    def writer():
        for i in range(200):
            def _mut(entries, i=i):
                entries.append({"id": f"x-w{i:04x}", "title": f"n{i}",
                                "status": "ready", "project": "fno", "domain": "code"})
                return entries
            locked_mutate_graph(scratch, _mut)
        stop.set()

    def reader():
        while not stop.is_set():
            try:
                entries = read_graph_strict(scratch)
            except GraphUnreadableError as e:
                # An atomic os.replace never yields a partial file, so a present
                # node must never read as unreadable here either.
                read_failures.append(e)
                continue
            match = resolve_node("x-keep", entries)
            if match.kind != "exact":
                misses.append(match.kind)

    threads = [threading.Thread(target=writer)] + [
        threading.Thread(target=reader) for _ in range(3)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert misses == [], f"{len(misses)} false negative(s) for a present node: {misses[:5]}"
    assert read_failures == [], f"{len(read_failures)} spurious read failure(s)"


def test_ac3fr_resolution_path_does_not_swallow_corruption(scratch):
    # Anti-regression guard: if the resolution reader is reverted to
    # read_graph's soft swallow, an unreadable graph would resolve to [] and a
    # present-node lookup would silently miss. The strict reader must RAISE.
    scratch.write_text("{ corrupt not json")
    with pytest.raises(GraphUnreadableError):
        read_graph_strict(scratch)
    # And the soft reader still swallows -- proving the two paths are distinct
    # and the resolution path is the strict one, not the soft one.
    assert read_graph(scratch) == []
