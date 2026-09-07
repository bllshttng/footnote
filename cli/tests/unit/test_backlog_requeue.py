"""Tests for `fno backlog requeue` and the `unclaim` read-back refusal.

A node whose worker died mid-do stays `in_progress` through its open do row
alone: `locked_by` OR an open do window derives in_progress
(graph_store.rs recompute_statuses). `requeue` proves the worker dead,
settles the row, and reports where the derivation landed; `unclaim` now
refuses to print success over a wedge it did not clear.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """A fresh empty graph.json; monkeypatches fno.graph constants to use it."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


@pytest.fixture
def claims_root(tmp_path, monkeypatch) -> Path:
    """Route node: claims into a tmp dir so seeding/asserting locks is hermetic."""
    root = tmp_path / "claims_home"
    root.mkdir()
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(root))
    return root


NODE_ID = "ab-4f44feed"
DEAD_SESSION = "5d67aad9-dead-beef"


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _out(result) -> str:
    return result.output + (getattr(result, "stderr", None) or "")


def _wedged_node(**over) -> dict:
    """in_progress via an open do row alone: lock free, no PR (the x-4f44 shape)."""
    node = {
        "id": NODE_ID,
        "title": "Wedged thing",
        "slug": "wedged-thing",
        "domain": "code",
        "project": "p",
        "plan_path": "internal/plan.md",  # so the settled node derives ready
        "status": "in_progress",
        "locked_by": None,
        "locked_at": None,
        "pr_number": None,
        "sessions": [{
            "phase": "do",
            "harness": "claude",
            "session_id": DEAD_SESSION,
            "started_at": "2026-09-05T06:11:05Z",
        }],
    }
    node.update(over)
    return node


def _dead_truth(monkeypatch, state="stalled") -> None:
    monkeypatch.setattr(
        "fno.agents.session_truth.resolve_session_truth",
        lambda handle, **kw: {
            "handle": handle,
            "state": state,
            "last_activity_age_s": 18000,
            "last_event_at": "2026-09-05T06:11:05Z",
        },
    )


def _acquire(key: str, holder: str, pid: int, root: Path) -> None:
    from fno.claims.core import acquire_claim
    acquire_claim(key=key, holder=holder, pid=pid, root=root)


# -- AC1-HP: requeue settles the wedged node ---------------------------------


def test_ac1_requeue_settles_wedged_node(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 0, _out(result)
    node = _read(tmp_graph)[0]
    assert node["status"] != "in_progress"
    assert node["status"] == "ready"
    # The do row is removed, not just stamped: nothing reads as an open window.
    assert node["sessions"] == []
    assert DEAD_SESSION in result.output


def test_ac1_requeue_json_receipt(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID, "--json"])
    assert result.exit_code == 0, _out(result)
    receipt = json.loads(result.output)
    assert receipt["node_id"] == NODE_ID
    assert receipt["status_before"] == "in_progress"
    assert receipt["status_after"] != "in_progress"
    assert receipt["settled"][0]["session_id"] == DEAD_SESSION
    assert receipt["settled"][0]["state"] == "stalled"


# -- AC2-EDGE: only free or stale claims may requeue --------------------------


def test_ac2_requeue_refuses_live_claim(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch)
    _acquire(f"node:{NODE_ID}", "target-session:live-one", pid=os.getpid(), root=claims_root)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code != 0
    assert "live-one" in _out(result)
    node = _read(tmp_graph)[0]
    assert node["status"] == "in_progress"
    assert node["sessions"][0].get("ended_at") is None


@pytest.mark.parametrize("state", ["suspect", "corrupted"])
def test_ac2_requeue_refuses_non_free_states(tmp_graph, claims_root, monkeypatch, state):
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch)
    import fno.claims.core as cc
    real_status = cc.claim_status

    def fake_status(key, **kw):
        s = dict(real_status(key, **kw))
        if key == f"node:{NODE_ID}":
            s.update(state=state, holder="target-session:held")
        return s

    monkeypatch.setattr("fno.claims.core.claim_status", fake_status)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code != 0
    assert state in _out(result)
    assert "held" in _out(result)
    node = _read(tmp_graph)[0]
    assert node["status"] == "in_progress"
    assert node["sessions"][0].get("ended_at") is None


# -- AC3-EDGE: a warm worker still owns the do window -------------------------


def test_ac3_requeue_refuses_warm_session(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch, state="working")
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code != 0
    assert DEAD_SESSION in _out(result)
    assert "working" in _out(result)
    node = _read(tmp_graph)[0]
    assert node["status"] == "in_progress"
    assert node["sessions"][0].get("ended_at") is None


# -- AC4-HP / AC5-EDGE: unclaim earns its success line ------------------------


def test_ac4_unclaim_refuses_wedge_it_did_not_clear(tmp_graph, claims_root):
    _seed(tmp_graph, [_wedged_node()])
    result = runner.invoke(app, ["backlog", "unclaim", NODE_ID])
    assert result.exit_code != 0
    assert "Unclaimed" not in _out(result)
    assert "in_progress" in _out(result)
    assert "fno backlog requeue" in _out(result)


def test_ac5_unclaim_clears_lock_held_in_progress(tmp_graph, claims_root):
    _seed(tmp_graph, [_wedged_node(
        status="in_progress",
        locked_by="target-session:gone",
        locked_at="2026-09-05T06:00:00Z",
        sessions=[],
    )])
    result = runner.invoke(app, ["backlog", "unclaim", NODE_ID])
    assert result.exit_code == 0, _out(result)
    assert "Unclaimed" in result.output
    node = _read(tmp_graph)[0]
    assert node["status"] == "ready"


# -- AC6-EDGE: a node with a PR is in_review, not requeueable -----------------


def test_ac6_requeue_never_clears_a_pr(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_wedged_node(status="in_review", pr_number=1547)])
    _dead_truth(monkeypatch)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code != 0
    assert "in_review" in _out(result)
    node = _read(tmp_graph)[0]
    assert node["pr_number"] == 1547
    assert node["status"] == "in_review"


# -- review round 1: the mid-verb claim race and the update sibling ------------


def test_requeue_aborts_when_claim_lands_mid_verb(tmp_graph, claims_root, monkeypatch):
    """A manual claim that lands between requeue's read and its clear must
    survive: the clear aborts instead of yanking a live late claim."""
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch)
    import fno.graph.store as gs
    real_mutate = gs.locked_mutate_graph

    def racing_mutate(path, mutator):
        def injected(entries):
            for e in entries:
                if e.get("id") == NODE_ID:
                    e["locked_by"] = "target-session:late"
            return mutator(entries)
        return real_mutate(path, injected)

    monkeypatch.setattr(gs, "locked_mutate_graph", racing_mutate)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code != 0
    assert "target-session:late" in _out(result)


def test_update_null_locked_by_refuses_wedge(tmp_graph):
    """update --locked-by null earns its Updated line the same way unclaim
    does: an open do row holds in_progress, so the receipt names requeue."""
    _seed(tmp_graph, [_wedged_node()])
    result = runner.invoke(app, ["backlog", "update", NODE_ID, "--locked-by", "null"])
    assert result.exit_code != 0
    assert "Updated" not in _out(result)
    assert "in_progress" in _out(result)
    assert "fno backlog requeue" in _out(result)


def test_update_null_locked_by_clears_lock_alone(tmp_graph):
    """The discrimination case: locked_by alone held the node, so the clear
    transitions it and the Updated line prints."""
    _seed(tmp_graph, [_wedged_node(
        locked_by="target-session:gone",
        locked_at="2026-09-05T06:00:00Z",
        sessions=[],
    )])
    result = runner.invoke(app, ["backlog", "update", NODE_ID, "--locked-by", "null"])
    assert result.exit_code == 0, _out(result)
    assert "Updated" in result.output
    assert _read(tmp_graph)[0]["status"] == "ready"
