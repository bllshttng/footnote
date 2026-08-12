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
import subprocess
import sys
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
    # Bound by the ceiling itself (it sleeps between attempts, so at most
    # _RETRY_ATTEMPTS - 1 times) rather than a magic number that drifts.
    assert 1 <= sleeps["n"] <= load_mod._RETRY_ATTEMPTS - 1


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


def test_present_but_invalid_sidecar_warns_before_reblessing(tmp_path, capsys):
    # A present-but-garbage sidecar silently disables corruption detection; warn
    # before re-blessing it (distinct from a legitimately-absent first run).
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [{"id": "x-a"}]}) + "\n")
    _sidecar_path(g).write_text("deadbeef")
    load_graph(g)
    assert "not a valid sha256" in capsys.readouterr().err


def test_absent_sidecar_first_run_does_not_warn(tmp_path, capsys):
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [{"id": "x-a"}]}) + "\n")
    load_graph(g)  # no sidecar present: normal first contact, no warning
    assert capsys.readouterr().err == ""


def test_ac1fr_transient_recovery_is_observable_under_fno_debug(tmp_path, monkeypatch, capsys):
    # AC1-FR: the recovery must not be entirely invisible. Under FNO_DEBUG the
    # retry emits a line so the transient window is observable.
    g = tmp_path / "graph.json"
    _write_consistent(g, [{"id": "x-a"}])
    g.write_text(json.dumps({"entries": [{"id": "x-a"}, {"id": "x-b"}]}) + "\n")
    correct = hashlib.sha256(g.read_bytes()).hexdigest()
    monkeypatch.setenv("FNO_DEBUG", "1")

    def fake_sleep(_s):
        _sidecar_path(g).write_text(correct + "\n")

    monkeypatch.setattr(load_mod.time, "sleep", fake_sleep)
    load_graph(g)
    err = capsys.readouterr().err
    assert "hash mismatch" in err and "retry" in err.lower()


# --- AC3-HP: concurrent writers + readers, zero false corruption ---

# Readers run as separate PROCESSES, which is what production does and what
# this test spent three commits failing to simulate with threads.
#
# With thread readers the GIL made the writer's two-write window as long as the
# scheduler wanted. On a loaded CI runner three readers squeezed the writer
# across both renames and held the window past load_graph's retry ceiling
# (12 attempts x 23ms = ~253ms, against a ~4.8ms window in production), so the
# test surfaced false corruption over a healthy graph. Three commits answered
# that by widening a reader yield: 0.0002s, then 0.002s, then a fourth failure
# on this branch. Every one of them compensated for the artifact rather than
# removing it, and the second commit message already named the reason it could
# not be removed that way - production readers are separate processes.
#
# They are separate processes here now. No yield, no tuning knob, and nothing
# left to widen on the next loaded runner.
_READER = r"""
import json, sys, time
from pathlib import Path
from fno.graph.load import GraphCorruptionError, load_graph

g, stop, out = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
false_corruption = missing_seed = loads = most = 0
deadline = time.monotonic() + 120  # never outlive a dead parent
while not stop.exists() and time.monotonic() < deadline:
    try:
        entries = load_graph(g)
    except GraphCorruptionError:
        false_corruption += 1
        continue
    loads += 1
    most = max(most, len(entries))
    if not any(e.get("id") == "x-keep" for e in entries):
        missing_seed += 1
out.write_text(json.dumps({
    "false_corruption": false_corruption,
    "missing_seed": missing_seed,
    "loads": loads,
    "most": most,
}))
"""


def test_ac3hp_concurrent_writes_never_surface_corruption(tmp_path):
    from fno.graph.store import locked_mutate_graph

    g = tmp_path / "graph.json"
    # Seed a present node the readers resolve throughout.
    def _seed(entries):
        entries.append({"id": "x-keep", "title": "keep", "status": "ready",
                        "project": "fno", "domain": "code"})
        return entries

    locked_mutate_graph(g, _seed)

    stop = tmp_path / "STOP"
    readers = []
    for n in range(3):
        out = tmp_path / f"reader{n}.json"
        readers.append((
            subprocess.Popen(
                [sys.executable, "-c", _READER, str(g), str(stop), str(out)]
            ),
            out,
        ))
    try:
        for i in range(200):
            def _mut(entries, i=i):
                entries.append({"id": f"x-w{i:04x}", "title": f"n{i}",
                                "status": "ready", "project": "fno", "domain": "code"})
                return entries
            locked_mutate_graph(g, _mut)
    finally:
        stop.write_text("")
        for proc, _ in readers:
            proc.wait(timeout=60)

    reports = []
    for _, out in readers:
        assert out.exists(), "a reader process died before reporting"
        reports.append(json.loads(out.read_text()))

    # Positive controls first. Zero errors from a reader that crashed on import
    # is the absence this whole gate exists to refuse, and it looks exactly
    # like a clean run.
    assert all(r["loads"] > 0 for r in reports), f"a reader never read: {reports}"
    assert max(r["most"] for r in reports) > 1, (
        f"no reader observed the writer's appends: {reports}"
    )

    bad = [r for r in reports if r["false_corruption"] or r["missing_seed"]]
    assert not bad, f"false corruption or a lost seed: {bad}"


def test_ac3hp_genuine_corruption_still_raises(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    _write_consistent(g, [{"id": "x-a"}])
    # Deliberately set a wrong hash and leave it: a real corruption must raise.
    _sidecar_path(g).write_text("0" * 64 + "\n")
    monkeypatch.setattr(load_mod.time, "sleep", lambda _s: None)
    with pytest.raises(GraphCorruptionError):
        load_graph(g)
