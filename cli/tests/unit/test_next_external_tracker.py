"""Joined external-backend selection for `backlog next` / `backlog ready`.

Task 3.1's contract: ``list_open()`` exactly once, sidecar joined per open id
BEFORE the filters run, closed history never requested, footnote ranking
applied after the join, backend failures fail closed, and graph/external
winner parity. Every test points the seam at a contradictory local graph file:
if selection ever answered from it, the sentinels give the test away.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


class FakeTracker:
    """A NodeTracker fake over seam-only facts, with call accounting."""

    name = "fake-external"

    def __init__(self, rows, *, fail_list_open=False, fail_read_ids=()):
        from fno.tracker.types import TrackerCandidate, TrackerState

        self._T, self._S = TrackerCandidate, TrackerState
        self._rows = rows
        self._fail_list_open = fail_list_open
        self._fail_read_ids = set(fail_read_ids)
        self.list_open_calls = 0
        self.read_calls: list[str] = []

    def read(self, id):
        from fno.tracker.types import NodeNotFound

        self.read_calls.append(id)
        if id in self._fail_read_ids:
            raise RuntimeError(f"backend read blew up for {id}")
        for r in self._rows:
            if r["id"] == id:
                return self._T(
                    id=r["id"], title=r.get("title"),
                    state=self._S(r.get("state", "open")),
                    parent=r.get("parent"),
                    blocked_by=r.get("blocked_by", []),
                )
        raise NodeNotFound(id)

    def list_open(self):
        self.list_open_calls += 1
        if self._fail_list_open:
            raise RuntimeError("network partition")
        out = []
        for r in self._rows:
            if r.get("state", "open") == "open":
                out.append(self._T(
                    id=r["id"], title=r.get("title"), state=self._S.open,
                    parent=r.get("parent"), blocked_by=r.get("blocked_by", []),
                    priority=r.get("priority", "p2"),
                    rank=r.get("rank"), created_at=r.get("created_at"),
                ))
        return out

    def close(self, id):
        raise AssertionError("close is not part of selection")


def _wire(monkeypatch, tmp_path, rows, sidecars, **tracker_kwargs):
    """Point tracker, sidecar store, claims, and the local graph at fakes."""
    tracker = FakeTracker(rows, **tracker_kwargs)
    monkeypatch.setattr("fno.tracker.get_tracker", lambda *a, **k: tracker)
    sidecar_dir = tmp_path / "sidecars"
    sidecar_dir.mkdir(exist_ok=True)
    for sid, fields in sidecars.items():
        payload = {"id": sid}
        payload.update(fields)
        (sidecar_dir / f"{sid}.json").write_text(json.dumps(payload))
    import fno.tracker.sidecar as sidecar_store

    monkeypatch.setattr(sidecar_store, "sidecar_path",
                        lambda i: sidecar_dir / f"{i}.json")
    # The contradictory local graph: every value here must never surface.
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [
        {"id": r["id"], "title": "GRAPH-SENTINEL", "cwd": "/graph-cwd",
         "status": "ready", "priority": "p0"}
        for r in rows
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    return tracker, g


def _rows_basic():
    return [
        {"id": "EXT-hi", "title": "High prio leaf", "priority": "p1",
         "created_at": "2026-08-01T00:00:00Z"},
        {"id": "EXT-lo", "title": "Low prio leaf", "priority": "p3",
         "created_at": "2026-08-02T00:00:00Z"},
    ]


def test_next_joins_once_and_ranks_over_the_open_set(tmp_path, monkeypatch):
    """AC4-HP: list_open exactly once; the winner is tracker-ranked (p1 beats
    p3) with the title coming from the tracker, never GRAPH-SENTINEL."""
    tracker, _g = _wire(monkeypatch, tmp_path, _rows_basic(), {
        "EXT-hi": {"plan_path": "/plans/hi.md", "cwd": "/ext/hi"},
        "EXT-lo": {"plan_path": "/plans/lo.md", "cwd": "/ext/lo"},
    })
    r = runner.invoke(app, ["backlog", "next"], catch_exceptions=False)
    assert r.exit_code == 0, r.output
    doc = json.loads(r.output)
    assert doc["id"] == "EXT-hi"
    assert doc["title"] == "High prio leaf"
    assert doc["cwd"] == "/ext/hi"
    assert tracker.list_open_calls == 1


def test_next_excludes_pr_and_batch_and_contained_before_ranking(
    tmp_path, monkeypatch
):
    """AC4-HP: the sidecar join runs BEFORE the filters - PR-in-flight, batch,
    and containment facts are all sidecar-owned and all exclude."""
    rows = _rows_basic() + [
        {"id": "EXT-pr", "title": "In review", "priority": "p0",
         "created_at": "2026-08-01T00:00:00Z"},
        {"id": "EXT-bat", "title": "Batched", "priority": "p0",
         "created_at": "2026-08-01T00:00:00Z"},
        {"id": "EXT-con", "title": "Contained", "priority": "p0",
         "created_at": "2026-08-01T00:00:00Z"},
    ]
    _wire(monkeypatch, tmp_path, rows, {
        "EXT-hi": {"plan_path": "/plans/hi.md"},
        "EXT-lo": {"plan_path": "/plans/lo.md"},
        "EXT-pr": {"plan_path": "/plans/pr.md", "pr_number": 7},
        "EXT-bat": {"plan_path": "/plans/b.md", "batch": "batch-1"},
        "EXT-con": {"plan_path": "/plans/c.md", "contained_in": "EXT-hi"},
    })
    r = runner.invoke(app, ["backlog", "next"], catch_exceptions=False)
    assert r.exit_code == 0, r.output
    doc = json.loads(r.output)
    # p0 rows are all excluded by sidecar facts; the p1 leaf wins.
    assert doc["id"] == "EXT-hi"


def test_next_fail_closed_names_the_backend(tmp_path, monkeypatch):
    """AC6-ERR: a failing backend exits nonzero with the backend named and
    never answers from the local graph (whose rows all say ready/p0)."""
    _wire(monkeypatch, tmp_path, _rows_basic(), {},
          fail_list_open=True)
    r = runner.invoke(app, ["backlog", "next"], catch_exceptions=False)
    assert r.exit_code == 1
    assert "fake-external" in r.output
    assert r.output.strip() != "null"


def test_next_fail_closed_names_the_id_on_sidecar_fault(tmp_path, monkeypatch):
    """AC6-ERR: a failing per-id sidecar read names the id; no silent
    selection of a different node."""
    rows = _rows_basic()
    tracker, _g = _wire(monkeypatch, tmp_path, rows, {
        "EXT-hi": {"plan_path": "/plans/hi.md"},
    })
    # Make the second candidate's sidecar unreadable at the store level by
    # failing its file read: a directory in place of the file raises on read.
    import fno.tracker.sidecar as sidecar_store

    real_load = sidecar_store.load
    monkeypatch.setattr(sidecar_store, "load", lambda i: real_load(i))
    (Path(sidecar_store.sidecar_path("EXT-lo").parent) / "EXT-lo.json").mkdir()

    r = runner.invoke(app, ["backlog", "next"], catch_exceptions=False)
    assert r.exit_code == 1
    assert "EXT-lo" in r.output


def test_next_winner_parity_between_backends(tmp_path, monkeypatch):
    """AC5-EDGE: with the same seam-carried facts, the graph backend and the
    external fake choose the same winner; priority stays tracker-owned and
    never crosses in a sidecar."""
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [
        {"id": "ab-aaa00001", "title": "Leaf under epic", "status": "ready",
         "priority": "p2", "parent": "ab-eee00001",
         "created_at": "2026-08-01T00:00:00Z", "plan_path": "/p/1.md"},
        {"id": "ab-loose001", "title": "Loose p1", "status": "ready",
         "priority": "p1", "created_at": "2026-08-02T00:00:00Z",
         "plan_path": "/p/2.md"},
        {"id": "ab-eee00001", "title": "The epic", "status": "ready",
         "priority": "p3", "created_at": "2026-07-01T00:00:00Z"},
    ]}), encoding="utf-8")
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))

    r = runner.invoke(app, ["backlog", "next", "-A"], catch_exceptions=False)
    assert r.exit_code == 0, r.output
    graph_winner = json.loads(r.output)["id"]

    rows = [
        {"id": "ab-aaa00001", "title": "Leaf under epic", "priority": "p2",
         "parent": "ab-eee00001", "created_at": "2026-08-01T00:00:00Z"},
        {"id": "ab-loose001", "title": "Loose p1", "priority": "p1",
         "created_at": "2026-08-02T00:00:00Z"},
        {"id": "ab-eee00001", "title": "The epic", "priority": "p3",
         "created_at": "2026-07-01T00:00:00Z"},
    ]
    _wire(monkeypatch, tmp_path, rows, {
        "ab-aaa00001": {"plan_path": "/p/1.md"},
        "ab-loose001": {"plan_path": "/p/2.md"},
    })
    r2 = runner.invoke(app, ["backlog", "next", "-A"], catch_exceptions=False)
    assert r2.exit_code == 0, r2.output
    external_winner = json.loads(r2.output)["id"]

    assert external_winner == graph_winner
    # The epic is a container (someone's parent) and never wins selection.
    assert external_winner != "ab-eee00001"


def test_next_starvation_receipts_explain_the_joined_denominator(
    tmp_path, monkeypatch
):
    """A null result explains itself over the ACTUAL external candidates, not
    the local graph rows."""
    plan = tmp_path / "design.md"
    plan.write_text("---\ntitle: Not blueprinted\nstatus: design\n---\n# D\n")
    rows = [{"id": "EXT-des", "title": "Only candidate", "priority": "p1",
             "created_at": "2026-08-01T00:00:00Z"}]
    _wire(monkeypatch, tmp_path, rows, {
        "EXT-des": {"plan_path": str(plan)},
    })
    r = runner.invoke(app, ["backlog", "next"], catch_exceptions=False)
    assert r.exit_code == 0
    # stdout carries only the node-or-null contract; the receipt rides stderr
    # (mixed into .output by this runner, so assert on the last line + text).
    assert r.output.strip().splitlines()[-1] == "null"
    assert "EXT-des" in r.output  # the receipt names the real external id
    assert "design" in r.output


def test_next_claim_uses_the_claims_subsystem_not_the_graph(
    tmp_path, monkeypatch
):
    """--claim under an external backend acquires node:<id> through the claims
    subsystem: the local graph is never written and no claim pointer lands in
    tracker or sidecar."""
    rows = _rows_basic()
    tracker, g = _wire(monkeypatch, tmp_path, rows, {
        "EXT-hi": {"plan_path": "/plans/hi.md"},
        "EXT-lo": {"plan_path": "/plans/lo.md"},
    })
    before = g.read_text()

    r = runner.invoke(
        app, ["backlog", "next", "--claim", "sess-ext-1"],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output
    doc = json.loads(r.output)
    assert doc["id"] == "EXT-hi"
    # The graph file is untouched.
    assert g.read_text() == before
    # The claim exists in the claims dir under the opaque id.
    claims_root = tmp_path / "claims"
    locks = list(claims_root.rglob("*EXT-hi*"))
    assert locks, f"no claim lock for EXT-hi under {claims_root}"
    # No claim pointer in the sidecar.
    sc = json.loads((Path(str(claims_root)).parent / "sidecars" / "EXT-hi.json").read_text())
    assert "locked_by" not in sc
