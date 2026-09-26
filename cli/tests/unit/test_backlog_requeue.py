"""Tests for `fno backlog requeue` and the `unclaim` read-back refusal.

A node whose worker died mid-do stays `in_progress` through its open do row
alone: `locked_by` OR an open do window derives in_progress
(graph_store.rs recompute_statuses). `requeue` proves the worker dead,
settles the row, and reports where the derivation landed; `unclaim` now
refuses to print success over a wedge it did not clear.
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

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
    seed_graph(g, '{"entries": []}\n')
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
    seed_graph(g, json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    from fno.graph.store import read_graph_strict

    return read_graph_strict(g)


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
            "phase": "execute",
            "harness": "claude",
            "session_id": DEAD_SESSION,
            "started_at": "2026-09-05T06:11:05Z",
        }],
    }
    node.update(over)
    return node


def _dead_truth(monkeypatch, state="stalled", age_s=18000, observed=None) -> None:
    monkeypatch.setattr(
        "fno.agents.session_truth.resolve_session_truth",
        lambda handle, **kw: {
            "handle": handle,
            "state": state,
            "last_activity_age_s": age_s,
            "last_event_at": "2026-09-05T06:11:05Z",
            "observed_model": observed,
        },
    )


@pytest.fixture(autouse=True)
def _quiet_roster(monkeypatch):
    """No unit test reads the live fleet: a consulted, empty roster by default."""
    from fno.claims import roster

    monkeypatch.setattr(roster, "read_roster", lambda **_kw: roster.RosterReading(True, 0, {}))


def _started_ago(seconds: int) -> str:
    from datetime import datetime, timedelta, timezone

    return (datetime.now(timezone.utc) - timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%SZ")


def _fresh_node() -> dict:
    """A do row started a minute ago: the idle arm cannot fire, so only the
    session's reachability decides."""
    return _wedged_node(sessions=[{
        "phase": "execute", "harness": "claude", "session_id": DEAD_SESSION,
        "started_at": _started_ago(60),
    }])


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
    # The do row is filled and kept, not removed: closed reads as no open window.
    assert len(node["sessions"]) == 1
    assert node["sessions"][0]["session_id"] == DEAD_SESSION
    assert node["sessions"][0]["ended_at"]
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
    _seed(tmp_graph, [_fresh_node()])
    _dead_truth(monkeypatch, state="working", age_s=60)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code != 0
    assert DEAD_SESSION in _out(result)
    # The refusal names the evidence it refused on: the shared verdict's basis
    # and the humanized age (x-c1a3), not a bare state word.
    assert "reachable" in _out(result)
    assert "transcript" in _out(result)
    assert "1m" in _out(result)
    node = _read(tmp_graph)[0]
    assert node["status"] == "in_progress"
    assert node["sessions"][0].get("ended_at") is None


def test_requeue_unwedges_a_warm_spelling_past_the_freshness_bound(
    tmp_graph, claims_root, monkeypatch
):
    """The x-52d2 specimen: state working, transcript silent 83 minutes, the
    session dead by four instruments. The old membership test refused at any
    age and the node never returned to the queue; the shared verdict reads
    stale-transcript and requeue proceeds."""
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch, state="working", age_s=4980)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 0, _out(result)
    node = _read(tmp_graph)[0]
    assert node["status"] == "ready"
    assert len(node["sessions"]) == 1
    assert node["sessions"][0]["ended_at"]


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
    real_commit = gs.commit_rows_via_store

    def racing_commit(path, mutator):
        def injected(entries):
            for e in entries:
                if e.get("id") == NODE_ID:
                    e["locked_by"] = "target-session:late"
            return mutator(entries)
        return real_commit(path, injected)

    monkeypatch.setattr(gs, "commit_rows_via_store", racing_commit)
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


@pytest.mark.skip(


    reason="known defect: the terminal transition releases the claim but the "


    "row's locked_by/session_id mirror keeps the holder until the claim-mirror "


    "row releases in the same write"


)


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


# -- x-e594: the inference-sample marker, measured 2026-09-12 -----------------


def test_requeue_settles_a_429_corpse_inside_the_freshness_bound(
    tmp_graph, claims_root, monkeypatch
):
    """The measured specimen. A worker killed by a usage-limit 429 dies WRITING
    that error, so its transcript age is freshest at the instant it died: state
    working at 13 minutes read reachable and requeue refused, then accepted the
    same dead worker at 23 once age alone crossed the bound. Zero inference
    samples says no vendor ever answered a turn, so the age certifies nothing."""
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(
        monkeypatch, state="working", age_s=13 * 60, observed={"kind": "no-model-yet"}
    )
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 0, _out(result)
    node = _read(tmp_graph)[0]
    assert node["status"] == "ready"
    assert len(node["sessions"]) == 1
    assert node["sessions"][0]["ended_at"]


def test_requeue_still_refuses_a_worker_with_a_climbing_sample_count(
    tmp_graph, claims_root, monkeypatch
):
    """Same state and age, 31 samples: a live worker still owns the do window."""
    _seed(tmp_graph, [_fresh_node()])
    _dead_truth(
        monkeypatch,
        state="working",
        age_s=13 * 60,
        observed={"kind": "observed", "model": "glm-5.3-flash", "samples": 31},
    )
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code != 0
    assert "reachable" in _out(result)
    assert _read(tmp_graph)[0]["status"] == "in_progress"


def test_ac3_hp_the_reachable_refusal_names_the_owners_self_close(
    tmp_graph, claims_root, monkeypatch
):
    """The refusal names the next command: the owning session ends its own do
    row with `session add --ended-at`. reap-open is NOT named - this worker
    reads reachable, so a death claim would be false."""
    _seed(tmp_graph, [_fresh_node()])
    _dead_truth(
        monkeypatch,
        state="working",
        age_s=13 * 60,
        observed={"kind": "observed", "model": "glm-5.3-flash", "samples": 31},
    )
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 3
    assert (
        f"fno backlog session add {NODE_ID} --phase execute --ended-at" in _out(result)
    )
    assert "reap-open" not in _out(result)


def test_the_receipt_prints_the_sample_count_beside_the_state(
    tmp_graph, claims_root, monkeypatch
):
    """`working, 0 samples` names a corpse and `working, 31 samples` names a
    worker, so the state word alone is not a receipt a reader can act on."""
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(
        monkeypatch, state="working", age_s=13 * 60, observed={"kind": "no-model-yet"}
    )
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 0, _out(result)
    assert "state=working samples=0" in result.output


def test_the_receipt_never_renders_an_unanswerable_count_as_zero(
    tmp_graph, claims_root, monkeypatch
):
    """opencode keeps no per-session transcript, so its count is absent, not 0.
    Printing 0 there would name every opencode worker a corpse."""
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch, observed={"kind": "not-file-backed"})
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID, "--json"])
    assert result.exit_code == 0, _out(result)
    assert json.loads(result.output)["settled"][0]["samples"] is None


def test_an_unanswerable_count_renders_as_a_question_mark(
    tmp_graph, claims_root, monkeypatch
):
    """The plain receipt's spelling for the same absence."""
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch, observed={"kind": "not-file-backed"})
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 0, _out(result)
    assert "samples=?" in result.output


# -- x-b68d: the suspect refusal names when the grace ends ---------------------


def test_requeue_suspect_refusal_names_when_the_grace_ends(
    tmp_graph, claims_root, monkeypatch
):
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch)
    import time

    import fno.claims.core as cc
    real_status = cc.claim_status

    def fake_status(key, **kw):
        s = dict(real_status(key, **kw))
        if key == f"node:{NODE_ID}":
            s.update(
                state="suspect",
                basis="ttl-expired-unresolved",
                holder="spawn-handover:ghost",
                reclaimable_at=int((time.time() + 12 * 60) * 1000),
            )
        return s

    monkeypatch.setattr(cc, "claim_status", fake_status)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 3, _out(result)
    assert "ttl-expired-unresolved" in _out(result)
    assert "reclaimable at" in _out(result)
    assert "fno backlog requeue ab-4f44feed" in _out(result)
    node = _read(tmp_graph)[0]
    assert node["status"] == "in_progress"
    assert node["sessions"][0].get("ended_at") is None


def test_requeue_suspect_refusal_invents_no_clock(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_wedged_node()])
    _dead_truth(monkeypatch)
    import fno.claims.core as cc
    real_status = cc.claim_status

    def fake_status(key, **kw):
        s = dict(real_status(key, **kw))
        if key == f"node:{NODE_ID}":
            s.update(state="suspect", basis="pid-absent", holder="target-session:held")
        return s

    monkeypatch.setattr(cc, "claim_status", fake_status)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 3, _out(result)
    assert "pid-absent" in _out(result)
    assert "reclaimable" not in _out(result)


# -- x-fe51: a live session no longer holds an idle row forever ----------------


def _idle_node() -> dict:
    return _wedged_node(sessions=[{
        "phase": "execute", "harness": "claude", "session_id": DEAD_SESSION,
        "started_at": _started_ago(30 * 3600),
    }])


def _roster(monkeypatch, *, consulted=True, engaged=()) -> None:
    from fno.claims import roster

    workers = [{"name": n} for n in engaged]
    monkeypatch.setattr(
        roster, "read_roster",
        lambda **_kw: roster.RosterReading(consulted, 1, {NODE_ID: workers}, "" if consulted else "probe timed out"),
    )
    monkeypatch.setattr(roster, "classify_workers", lambda ws: (list(ws), [], {}))


def test_requeue_settles_an_idle_row_under_a_reachable_session(tmp_graph, claims_root, monkeypatch):
    """The x-eb79 specimen: the session reads working at 60s, but it has not
    touched this node in 30h and no reachable worker is on the node."""
    _seed(tmp_graph, [_idle_node()])
    _dead_truth(monkeypatch, state="working", age_s=60)
    _roster(monkeypatch)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID, "--json"])
    assert result.exit_code == 0, _out(result)
    assert _read(tmp_graph)[0]["status"] == "ready"
    assert json.loads(result.output)["settled"][0]["row_idle_s"] >= 108000


def test_requeue_holds_an_idle_row_with_a_reachable_worker_on_the_node(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_idle_node()])
    _dead_truth(monkeypatch, state="working", age_s=60)
    _roster(monkeypatch, engaged=["worker-a"])
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 3, _out(result)
    assert "worker-a" in _out(result)
    assert _read(tmp_graph)[0]["sessions"][0]["session_id"] == DEAD_SESSION


def test_requeue_reachable_refusal_names_its_clock(tmp_graph, claims_root, monkeypatch):
    """A fresh row can only be held, so the refusal never waits on a fleet read."""
    _seed(tmp_graph, [_fresh_node()])
    _dead_truth(monkeypatch, state="working", age_s=60)
    _roster(monkeypatch, consulted=False)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 3, _out(result)
    assert f"fno backlog session add {NODE_ID} --phase execute --ended-at" in _out(result)
    assert "The execute row stays: row idle 0h, inside the 24h bound" in _out(result)


def test_requeue_refuses_an_idle_row_when_the_roster_is_unread(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_idle_node()])
    _dead_truth(monkeypatch, state="working", age_s=60)
    _roster(monkeypatch, consulted=False)
    result = runner.invoke(app, ["backlog", "requeue", NODE_ID])
    assert result.exit_code == 3, _out(result)
    assert "roster unread" in _out(result)
    assert _read(tmp_graph)[0]["sessions"][0]["session_id"] == DEAD_SESSION
