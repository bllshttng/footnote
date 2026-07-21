"""US4: load_graph rides out the graph/sidecar two-write window.

graph.json and its .sha256 sidecar are written as two sequential atomic
replaces inside the write lock; readers take no lock. A reader landing between
the two writes sees new graph bytes against the old sidecar -- a hash mismatch
on a perfectly healthy graph (~4.8ms window on the live graph). A bounded retry
that re-reads BOTH files closes that window while a genuine corruption still
raises after the attempts are exhausted.
"""
from __future__ import annotations

import hashlib
import json
import threading
import time
from pathlib import Path

import pytest

import fno.graph.load as load_mod
from fno.graph.load import GraphCorruptionError, _sidecar_path, load_graph

# The retry ceiling: (attempts - 1) sleeps. Stated generously so a loaded CI box
# never flakes; the point is that exhaustion is bounded, not instantaneous.
_CEILING_S = 2.0


def _write_consistent(g: Path, entries: list[dict]) -> str:
    """Write graph + a matching sidecar; return the digest."""
    g.write_text(json.dumps({"entries": entries}) + "\n")
    digest = hashlib.sha256(g.read_bytes()).hexdigest()
    _sidecar_path(g).write_text(digest + "\n")
    return digest


# --- AC3-HP / AC1-FR: a transient mismatch recovers, no error, no .bak ---

def test_ac1fr_transient_mismatch_recovers_without_error(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    _write_consistent(g, [{"id": "x-a"}])
    # Simulate the window: new graph bytes land, sidecar not yet updated.
    g.write_text(json.dumps({"entries": [{"id": "x-a"}, {"id": "x-b"}]}) + "\n")
    correct = hashlib.sha256(g.read_bytes()).hexdigest()

    # On the first retry sleep, the "writer" finishes its sidecar write.
    def fake_sleep(_s):
        _sidecar_path(g).write_text(correct + "\n")

    monkeypatch.setattr(load_mod.time, "sleep", fake_sleep)
    entries = load_graph(g)
    assert {e["id"] for e in entries} == {"x-a", "x-b"}
    assert list(tmp_path.glob("*.bak*")) == []


# --- AC2-EDGE: a permanent mismatch raises, bounded, naming both digests ---

def test_ac2edge_permanent_mismatch_raises_bounded(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    _write_consistent(g, [{"id": "x-a"}])
    # New graph bytes with a sidecar that never catches up == real corruption.
    g.write_text(json.dumps({"entries": [{"id": "x-a"}, {"id": "x-b"}]}) + "\n")
    actual = hashlib.sha256(g.read_bytes()).hexdigest()
    monkeypatch.setattr(load_mod.time, "sleep", lambda _s: None)

    start = time.monotonic()
    with pytest.raises(GraphCorruptionError) as exc:
        load_graph(g)
    assert time.monotonic() - start < _CEILING_S
    # Names the expected + actual digests, as it does today.
    assert actual[:8] in str(exc.value)


def test_ac2edge_mismatch_retries_are_bounded(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    _write_consistent(g, [{"id": "x-a"}])
    g.write_text(json.dumps({"entries": [{"id": "x-a"}, {"id": "x-b"}]}) + "\n")
    sleeps = {"n": 0}
    monkeypatch.setattr(load_mod.time, "sleep", lambda _s: sleeps.__setitem__("n", sleeps["n"] + 1))
    with pytest.raises(GraphCorruptionError):
        load_graph(g)
    # A bounded number of retries -- never an unbounded wait-until-consistent.
    assert 1 <= sleeps["n"] <= 10


# --- AC4-EDGE (Errors): a broken sidecar is not graph corruption ---

def test_absent_sidecar_is_first_run_trust(tmp_path):
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [{"id": "x-a"}]}) + "\n")
    # No sidecar written: first contact, trust the file and write the sidecar.
    entries = load_graph(g)
    assert [e["id"] for e in entries] == ["x-a"]
    assert _sidecar_path(g).exists()


def test_empty_sidecar_is_not_corruption(tmp_path):
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [{"id": "x-a"}]}) + "\n")
    _sidecar_path(g).write_text("")  # empty == broken sidecar, not corruption
    entries = load_graph(g)
    assert [e["id"] for e in entries] == ["x-a"]


def test_truncated_sidecar_is_not_corruption(tmp_path):
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [{"id": "x-a"}]}) + "\n")
    _sidecar_path(g).write_text("deadbeef")  # not a 64-char sha256
    entries = load_graph(g)
    assert [e["id"] for e in entries] == ["x-a"]


# --- AC3-HP: concurrent writers + readers, zero false corruption ---

def test_ac3hp_concurrent_writes_never_surface_corruption(tmp_path, monkeypatch):
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    # Seed a present node the readers resolve throughout.
    from fno.graph.store import locked_mutate_graph

    def _seed(entries):
        entries.append({"id": "x-keep", "title": "keep", "status": "ready",
                        "project": "fno", "domain": "code"})
        return entries

    locked_mutate_graph(g, _seed)

    errors: list[Exception] = []
    stop = threading.Event()

    def writer():
        for i in range(200):
            def _mut(entries, i=i):
                entries.append({"id": f"x-w{i:04x}", "title": f"n{i}",
                                "status": "ready", "project": "fno", "domain": "code"})
                return entries
            locked_mutate_graph(g, _mut)
        stop.set()

    def reader():
        while not stop.is_set():
            try:
                entries = load_graph(g)
                # false negative check: the seeded node is always present
                assert any(e.get("id") == "x-keep" for e in entries)
            except GraphCorruptionError as e:
                errors.append(e)

    threads = [threading.Thread(target=writer)] + [
        threading.Thread(target=reader) for _ in range(3)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert errors == [], f"{len(errors)} false corruption(s): {errors[:2]}"


def test_ac3hp_genuine_corruption_still_raises(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    _write_consistent(g, [{"id": "x-a"}])
    # Deliberately set a wrong hash and leave it: a real corruption must raise.
    _sidecar_path(g).write_text("0" * 64 + "\n")
    monkeypatch.setattr(load_mod.time, "sleep", lambda _s: None)
    with pytest.raises(GraphCorruptionError):
        load_graph(g)
