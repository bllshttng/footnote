"""Tests for `fno backlog epic status <id>` (x-1124 K4).

One cross-project table over an epic's children: id/slug, project, status, live
worker (node:<id> claim holder), PR (node stamp). A `ready` child with no live
worker shows its most recent dispatch/skip/failure receipt inline - never a
blank cell (the silent failure this verb exists to kill). A `deferred` child
shows its consecutive-failure breaker streak.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner


def _node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "parent": None,
        "slug": node_id.replace("-", "") + "-slug",
        "title": f"title {node_id}",
        "type": "feature",
        "project": "fno",
        "cwd": None,
        "priority": "p2",
        "domain": "code",
        "blocked_by": [],
        "pr_number": None,
        "pr_url": None,
        "_status": "ready",
        "created_at": "2026-01-01T00:00:00+00:00",
    }
    base.update(overrides)
    return base


@pytest.fixture
def graph_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Temp graph.json wired into the CLI; children cwd -> tmp_path so their
    per-project events log resolves under the test dir. Returns tmp_path."""
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    def _write(entries):
        g.write_text(json.dumps({"entries": entries}) + "\n")

    return tmp_path, _write


def _invoke(args):
    from fno.cli import app

    return CliRunner().invoke(app, args)


def _write_events(cwd: Path, events: list[dict]) -> None:
    p = cwd / ".fno" / "events.jsonl"
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("".join(json.dumps(e) + "\n" for e in events))


# -- table: id / project / status / PR (Success Definition 4) --


def test_lists_children_with_status_project_pr(graph_env):
    tmp_path, write = graph_env
    write([
        _node("x-epic", type="epic", slug="the-epic"),
        _node("x-c1", parent="x-epic", _status="done", pr_number=460, cwd=str(tmp_path)),
        _node("x-c2", parent="x-epic", _status="ready", cwd=str(tmp_path)),
    ])
    r = _invoke(["backlog", "epic", "status", "x-epic"])
    assert r.exit_code == 0, r.output
    assert "x-c1" in r.output and "x-c2" in r.output
    assert "done" in r.output and "ready" in r.output
    assert "460" in r.output  # PR stamp surfaced
    assert "1/2" in r.output  # children_done/total rollup


# -- feature #2: ready + no worker shows a receipt inline --


def test_ready_child_shows_latest_receipt(graph_env):
    tmp_path, write = graph_env
    write([
        _node("x-epic", type="epic"),
        _node("x-c2", parent="x-epic", _status="ready", cwd=str(tmp_path)),
    ])
    _write_events(tmp_path, [
        {"ts": "2026-07-18T10:00:00Z", "type": "advance_skipped",
         "source": "backlog", "data": {"reason": "unmapped-project", "node_id": "x-c2"}},
    ])
    r = _invoke(["backlog", "epic", "status", "x-epic"])
    assert r.exit_code == 0, r.output
    assert "unmapped-project" in r.output


def test_ready_child_receipt_picks_newest(graph_env):
    tmp_path, write = graph_env
    write([
        _node("x-epic", type="epic"),
        _node("x-c2", parent="x-epic", _status="ready", cwd=str(tmp_path)),
    ])
    _write_events(tmp_path, [
        {"ts": "2026-07-18T10:00:00Z", "type": "advance_skipped",
         "source": "backlog", "data": {"reason": "walker-live", "node_id": "x-c2"}},
        {"ts": "2026-07-18T11:00:00Z", "type": "advance_failed",
         "source": "backlog", "data": {"error": "spawn boom", "node_id": "x-c2"}},
    ])
    r = _invoke(["backlog", "epic", "status", "x-epic"])
    assert "spawn boom" in r.output
    assert "walker-live" not in r.output  # superseded by the newer failure


# -- feature #2 (the silent-failure kill): no receipt -> explicit sentinel --


def test_ready_child_no_events_shows_no_receipt_found(graph_env):
    tmp_path, write = graph_env
    write([
        _node("x-epic", type="epic"),
        _node("x-c2", parent="x-epic", _status="ready", cwd=str(tmp_path)),
    ])
    r = _invoke(["backlog", "epic", "status", "x-epic"])
    assert r.exit_code == 0, r.output
    assert "no receipt found" in r.output


# -- feature #3: deferred child shows its breaker streak --


def test_deferred_child_shows_streak(graph_env):
    tmp_path, write = graph_env
    write([
        _node("x-epic", type="epic"),
        _node("x-c3", parent="x-epic", _status="deferred", cwd=str(tmp_path)),
    ])
    _write_events(tmp_path, [
        {"ts": "2026-07-18T09:00:00Z", "type": "node_failed", "source": "hook",
         "data": {"node_id": "x-c3"}},
        {"ts": "2026-07-18T10:00:00Z", "type": "node_failed", "source": "hook",
         "data": {"node_id": "x-c3"}},
    ])
    r = _invoke(["backlog", "epic", "status", "x-epic"])
    assert r.exit_code == 0, r.output
    assert "2" in r.output
    assert "streak" in r.output.lower()


# -- refuse a non-container by name --


def test_refuses_non_container(graph_env):
    tmp_path, write = graph_env
    write([
        _node("x-epic", type="epic"),
        _node("x-c2", parent="x-epic", _status="ready", cwd=str(tmp_path)),
    ])
    r = _invoke(["backlog", "epic", "status", "x-c2"])  # a leaf, not a container
    assert r.exit_code != 0
    assert "not" in r.output.lower() or "container" in r.output.lower() or "leaf" in r.output.lower()


def test_unknown_id_errors(graph_env):
    tmp_path, write = graph_env
    write([_node("x-epic", type="epic")])
    r = _invoke(["backlog", "epic", "status", "x-nope"])
    assert r.exit_code != 0


def test_childless_epic_shows_no_children(graph_env):
    """An `epic`-typed node with no children yet is queryable, not refused."""
    tmp_path, write = graph_env
    write([_node("x-epic", type="epic", slug="the-epic")])
    r = _invoke(["backlog", "epic", "status", "x-epic"])
    assert r.exit_code == 0, r.output
    assert "no children" in r.output.lower()


# -- accepts a slug --


def test_resolves_by_slug(graph_env):
    tmp_path, write = graph_env
    write([
        _node("x-epic", type="epic", slug="the-epic"),
        _node("x-c2", parent="x-epic", _status="ready", cwd=str(tmp_path)),
    ])
    r = _invoke(["backlog", "epic", "status", "the-epic"])
    assert r.exit_code == 0, r.output
    assert "x-c2" in r.output


# -- live worker: node:<id> claim holder surfaces --


def test_live_worker_shown(graph_env):
    tmp_path, write = graph_env
    write([
        _node("x-epic", type="epic"),
        _node("x-c2", parent="x-epic", _status="ready", cwd=str(tmp_path)),
    ])
    from fno.claims.core import acquire_claim, release_claim
    from fno.claims.io import claims_root_for

    key = "node:x-c2"
    root = claims_root_for(key)
    holder = "target-session:abc123"
    acquire_claim(key=key, holder=holder, pid=os.getpid(), root=root)
    try:
        r = _invoke(["backlog", "epic", "status", "x-epic"])
        assert r.exit_code == 0, r.output
        assert "target-session:abc123" in r.output or "abc123" in r.output
        # a claimed ready child is working, not idle -> no receipt lookup for it
        assert "no receipt found" not in r.output
    finally:
        # Claims live at the session-global root ($HOME redirect), which
        # persists across tests - release so the claim never leaks forward.
        release_claim(key, holder, root=root)


# -- JSON shape --


def test_json_output(graph_env):
    tmp_path, write = graph_env
    write([
        _node("x-epic", type="epic", slug="the-epic"),
        _node("x-c1", parent="x-epic", _status="done", pr_number=460, cwd=str(tmp_path)),
        _node("x-c2", parent="x-epic", _status="ready", cwd=str(tmp_path)),
    ])
    r = _invoke(["backlog", "epic", "status", "x-epic", "--json"])
    assert r.exit_code == 0, r.output
    payload = json.loads(r.output)
    assert payload["epic"] == "x-epic"
    assert payload["children_total"] == 2
    assert payload["children_done"] == 1
    kids = {c["id"]: c for c in payload["children"]}
    assert kids["x-c1"]["pr_number"] == 460
    assert kids["x-c2"]["status"] == "ready"
    assert kids["x-c2"]["receipt"] == "no receipt found"
