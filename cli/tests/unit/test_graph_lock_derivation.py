"""The graph flock is derived from the graph path, not a global /tmp constant.

Proves the invariant x-fb80 establishes: same graph file <-> one resolved
sibling lock, so concurrent writers to one graph serialize (no lost update)
and a scratch graph never contends with the real ~/.fno/graph.json.
"""
from __future__ import annotations

import json
import threading
import time
from pathlib import Path

import pytest

from fno.graph.store import _graph_lock_path, locked_mutate_graph


# --- AC2: the lock sits beside graph.json, resolved, never in /tmp ---

def test_lock_is_resolved_sibling(tmp_path):
    g = tmp_path / "graph.json"
    assert _graph_lock_path(g) == Path(str(g.resolve()) + ".lock")
    assert "/tmp/abilities-graph.lock" not in str(_graph_lock_path(g))


# --- AC4: aliased spellings of one file derive one lock inode ---

def test_aliased_paths_share_one_lock(tmp_path):
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    rel = Path(str(g)).parent / "." / "graph.json"
    assert _graph_lock_path(g) == _graph_lock_path(rel)


# --- AC5: a resolve failure degrades to the raw path, never crashes ---
# A symlink loop raises OSError (ELOOP) or RuntimeError depending on the Python
# version (3.13 raises neither, so a real loop can't exercise this branch there);
# mock both so the degrade path is proven portably.

@pytest.mark.parametrize("exc", [OSError("ELOOP"), RuntimeError("symlink loop")])
def test_resolve_failure_degrades_to_raw(monkeypatch, exc):
    def boom(self):
        raise exc

    monkeypatch.setattr(Path, "resolve", boom)
    raw = Path("some/graph.json")
    assert _graph_lock_path(raw) == Path("some/graph.json.lock")


# --- AC1: two concurrent writers to the SAME graph both land (no lost update) ---

def test_concurrent_writers_serialize(tmp_path):
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')

    def append(node_id):
        def mutator(entries):
            # Widen the read-modify-write window: without a mutually exclusive
            # lock, the second writer reads before the first writes and clobbers
            # it. Under the derived sibling lock the whole block is serialized.
            base = list(entries)
            time.sleep(0.05)
            base.append({"id": node_id, "title": node_id, "status": "intake"})
            return base
        return mutator

    threads = [
        threading.Thread(target=lambda i=i: locked_mutate_graph(g, append(f"x-{i}")))
        for i in range(2)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    ids = {e["id"] for e in json.loads(g.read_text())["entries"]}
    assert ids == {"x-0", "x-1"}, f"lost update: {ids}"
