"""Rich completion through NodeTracker.close under an external backend.

Task 4.1's contract (AC7/AC8): the shared gates run first, footnote-owned
rollups persist to the sidecar BEFORE the irreversible close, close(id) runs
exactly once, success prints only after it returns, a gate refusal preserves
its exit code with the item open, and a failed external close is loud and
retryable. A contradictory local graph file rides along: completion must
never answer from it.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


class DoneFakeTracker:
    name = "fake-external"

    def __init__(self, rows, *, fail_close_ids=()):
        from fno.tracker.types import TrackerCandidate, TrackerState

        self._T, self._S = TrackerCandidate, TrackerState
        self._rows = {r["id"]: r for r in rows}
        self._fail_close = set(fail_close_ids)
        self.close_calls: list[str] = []
        # Snapshot of each sidecar at its close moment, to prove rollups land
        # BEFORE the irreversible close.
        self.sidecar_at_close: dict[str, dict] = {}

    def read(self, id):
        from fno.tracker.types import NodeNotFound

        r = self._rows.get(id)
        if r is None:
            raise NodeNotFound(id)
        return self._T(
            id=r["id"], title=r.get("title"),
            state=self._S(r.get("state", "open")),
            parent=r.get("parent"), blocked_by=r.get("blocked_by", []),
        )

    def list_open(self):
        out = []
        for r in self._rows.values():
            if r.get("state", "open") == "open":
                out.append(self._T(
                    id=r["id"], title=r.get("title"), state=self._S.open,
                    parent=r.get("parent"), blocked_by=r.get("blocked_by", []),
                ))
        return out

    def close(self, id):
        self.close_calls.append(id)
        if id in self._fail_close:
            raise RuntimeError("backend 503")
        self._rows[id]["state"] = "closed"
        sc_path = self._sidecar_dir / f"{id}.json"
        if sc_path.exists():
            self.sidecar_at_close[id] = json.loads(sc_path.read_text())

    _sidecar_dir: Path = Path("/nonexistent")


def _wire(monkeypatch, tmp_path, rows, sidecars, **tracker_kwargs):
    tracker = DoneFakeTracker(rows, **tracker_kwargs)
    monkeypatch.setattr("fno.tracker.get_tracker", lambda *a, **k: tracker)
    sidecar_dir = tmp_path / "sidecars"
    sidecar_dir.mkdir(exist_ok=True)
    tracker._sidecar_dir = sidecar_dir
    for sid, fields in sidecars.items():
        payload = {"id": sid}
        payload.update(fields)
        (sidecar_dir / f"{sid}.json").write_text(json.dumps(payload))
    import fno.tracker.sidecar as sidecar_store

    monkeypatch.setattr(sidecar_store, "sidecar_path",
                        lambda i: sidecar_dir / f"{i}.json")
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [
        {"id": r["id"], "title": "GRAPH-SENTINEL", "completed_at": None}
        for r in rows
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setattr("fno.graph.cli._graph_path", lambda: g)
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    # No ledger -> rollup fills nothing (keeps the close-order assertion tight).
    from fno.graph import _constants as gc

    monkeypatch.setattr(gc, "LEDGER_JSON", tmp_path / "absent-ledger.json")
    return tracker, sidecar_dir


def _gh_state(state, url="https://github.com/o/r/pull/7"):
    from fno.graph._reconcile import PrMergeState

    return PrMergeState(number=7, state=state, url=url,
                        merged_at="2026-08-16T00:00:00Z" if state == "MERGED" else None)


def test_gates_then_sidecar_then_exactly_one_close(tmp_path, monkeypatch):
    """AC7-HP: merged evidence passes the gate, the sidecar carries the PR url
    BEFORE the close fires, close runs exactly once, success after."""
    rows = [{"id": "EXT-1", "title": "Shipped thing"}]
    tracker, sc_dir = _wire(monkeypatch, tmp_path, rows, {
        "EXT-1": {"pr_number": 7, "cwd": "/ext",
                  "pr_url": "https://github.com/o/r/pull/7"},
    })
    monkeypatch.setattr(
        "fno.graph.cli._done_gh_query",
        lambda pr, **kw: _gh_state("MERGED"),
    )

    r = runner.invoke(app, ["backlog", "done", "EXT-1"],
                      catch_exceptions=False)
    assert r.exit_code == 0, r.output
    assert "Marked EXT-1 done" in r.output
    assert tracker.close_calls == ["EXT-1"]
    # The sidecar already carried the evidencing url at close time.
    assert tracker.sidecar_at_close["EXT-1"]["pr_url"] == "https://github.com/o/r/pull/7"
    # The item is closed on the backend and untouched locally.
    assert tracker._rows["EXT-1"].get("state") == "closed"


def test_force_without_reason_is_a_usage_error_on_either_backend(
    tmp_path, monkeypatch
):
    """The `--force requires --reason` guard lives in the shared gate
    pipeline, so the external dispatch (which reaches the pipeline before any
    caller-side guard) still exits 2 with the item open instead of crashing
    on the pipeline's reason assertion or force-closing reasonless."""
    rows = [{"id": "EXT-1", "title": "Shipped thing"}]
    tracker, _ = _wire(monkeypatch, tmp_path, rows, {
        "EXT-1": {"pr_number": 7},
    })
    r = runner.invoke(app, ["backlog", "done", "EXT-1", "--force"],
                      catch_exceptions=False)
    assert r.exit_code == 2
    assert "--force requires --reason" in r.output
    assert tracker.close_calls == []
    assert tracker._rows["EXT-1"].get("state", "open") == "open"


def test_open_pr_refusal_keeps_item_open(tmp_path, monkeypatch):
    """AC8-ERR: an OPEN PR is not closing evidence - exit 5 (the graph-mode
    contract), close never called, item stays open."""
    rows = [{"id": "EXT-1", "title": "Awaiting merge"}]
    tracker, _ = _wire(monkeypatch, tmp_path, rows, {
        "EXT-1": {"pr_number": 7},
    })
    monkeypatch.setattr(
        "fno.graph.cli._done_gh_query",
        lambda pr, **kw: _gh_state("OPEN"),
    )

    r = runner.invoke(app, ["backlog", "done", "EXT-1"],
                      catch_exceptions=False)
    assert r.exit_code == 5
    assert tracker.close_calls == []
    assert tracker._rows["EXT-1"].get("state", "open") == "open"


def test_failed_external_close_is_loud_and_retryable(tmp_path, monkeypatch):
    """AC8-ERR: a backend close failure exits nonzero naming the backend, the
    item stays open, and a retry after the backend recovers closes it."""
    rows = [{"id": "EXT-1", "title": "Shipped thing"}]
    tracker, _ = _wire(monkeypatch, tmp_path, rows, {
        "EXT-1": {"pr_number": 7},
    }, fail_close_ids={"EXT-1"})
    monkeypatch.setattr(
        "fno.graph.cli._done_gh_query",
        lambda pr, **kw: _gh_state("MERGED"),
    )

    r = runner.invoke(app, ["backlog", "done", "EXT-1"],
                      catch_exceptions=False)
    assert r.exit_code == 1
    assert "fake-external" in r.output
    assert tracker._rows["EXT-1"].get("state", "open") == "open"

    tracker._fail_close.clear()
    r2 = runner.invoke(app, ["backlog", "done", "EXT-1"],
                       catch_exceptions=False)
    assert r2.exit_code == 0, r2.output
    assert tracker._rows["EXT-1"].get("state") == "closed"


def test_already_done_is_idempotent_without_gh(tmp_path, monkeypatch):
    rows = [{"id": "EXT-1", "state": "closed", "title": "Done already"}]
    tracker, _ = _wire(monkeypatch, tmp_path, rows, {"EXT-1": {}})
    called = []
    monkeypatch.setattr(
        "fno.graph.cli._done_gh_query",
        lambda pr, **kw: called.append(pr) or _gh_state("MERGED"),
    )
    r = runner.invoke(app, ["backlog", "done", "EXT-1"],
                      catch_exceptions=False)
    assert r.exit_code == 0
    assert "already done" in r.output
    assert tracker.close_calls == [] and not called


def test_cascade_closes_all_done_parents(tmp_path, monkeypatch):
    """The ancestor cascade rides tracker parent edges: the epic closes once
    its last open child closes."""
    rows = [
        {"id": "EXT-kid", "title": "Last child", "parent": "EXT-epic"},
        {"id": "EXT-epic", "title": "The epic"},
    ]
    tracker, _ = _wire(monkeypatch, tmp_path, rows, {
        "EXT-kid": {"pr_number": 7},
        "EXT-epic": {},
    })
    monkeypatch.setattr(
        "fno.graph.cli._done_gh_query",
        lambda pr, **kw: _gh_state("MERGED"),
    )
    r = runner.invoke(app, ["backlog", "done", "EXT-kid"],
                      catch_exceptions=False)
    assert r.exit_code == 0, r.output
    assert tracker.close_calls == ["EXT-kid", "EXT-epic"]


def test_fno_done_front_door_routes_through_the_same_terminal(
    tmp_path, monkeypatch
):
    """AC7's second front door: `fno done <id>` under an external backend runs
    the same terminal (gates, one close) rather than local graph resolution."""
    rows = [{"id": "EXT-1", "title": "Shipped thing"}]
    tracker, _ = _wire(monkeypatch, tmp_path, rows, {
        "EXT-1": {"pr_number": 7},
    })
    monkeypatch.setattr(
        "fno.graph.cli._done_gh_query",
        lambda pr, **kw: _gh_state("MERGED"),
    )
    from fno.done.cli import done_command

    r = runner.invoke(app, ["done", "EXT-1"], catch_exceptions=False)
    assert r.exit_code == 0, r.output
    assert tracker.close_calls == ["EXT-1"]


def test_backfill_sweep_refused_externally_without_touching_graph(
    tmp_path, monkeypatch
):
    """The graph-store sweep (enumerate done nodes, write rollups back) has no
    legal target under an external backend: it must refuse before any read and
    leave the contradictory local graph file byte-identical."""
    rows = [{"id": "EXT-1", "state": "closed", "title": "Done thing"}]
    _wire(monkeypatch, tmp_path, rows, {"EXT-1": {}})
    g = tmp_path / "graph.json"
    before = g.read_bytes()

    r = runner.invoke(app, ["done", "--backfill"], catch_exceptions=False)
    assert r.exit_code == 1
    assert "refused" in r.output
    assert g.read_bytes() == before
