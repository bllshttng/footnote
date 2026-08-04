"""Tests for ``fno.provenance.runtime_attempts`` (x-2ccd wave 3).

The runtime-attempt projection: live|suspect|stale|interrupted classification,
manifest+claim join, and fno_id dedup. Read-only: never writes a graph row.
"""
from __future__ import annotations

from pathlib import Path

from fno.provenance import runtime_attempts as ra
from fno.provenance.runtime_attempts import _classify, runtime_attempts

NODE = "x-test-prov"


def _manifest_for(node=NODE, fno_id="run-1", harness="claude", sid="sid-1"):
    return {
        "harness": harness,
        "harness_session_id": sid,
        "fno_id": fno_id,
        "graph_node_id": node,
        "target_claim_key": f"node:{node}",
        "target_claim_holder": f"target-session:{sid}",
    }


def _claim(state, holder="target-session:sid-1", pid=12345):
    return {"state": state, "holder": holder, "pid": pid}


def test_classify_matrix():
    # live claim -> live
    assert _classify("live", has_work=True, node_terminal=False) == "live"
    assert _classify("live", has_work=False, node_terminal=False) == "live"
    # dead claim + work + not delivered -> interrupted
    assert _classify("stale", has_work=True, node_terminal=False) == "interrupted"
    assert _classify("suspect", has_work=True, node_terminal=False) == "interrupted"
    assert _classify("free", has_work=True, node_terminal=False) == "interrupted"
    # dead claim + work BUT delivered (terminal) -> not interrupted (settled)
    assert _classify("stale", has_work=True, node_terminal=True) == "stale"
    # dead claim + no work -> stale / suspect
    assert _classify("stale", has_work=False, node_terminal=False) == "stale"
    assert _classify("suspect", has_work=False, node_terminal=False) == "suspect"


def test_runtime_live_claim_is_live(tmp_path, monkeypatch):
    monkeypatch.setattr(ra, "read_target_manifest", lambda wt: _manifest_for())
    monkeypatch.setattr(ra, "_commits_ahead", lambda wt, br: 3)
    rows = runtime_attempts(
        NODE, {"cwd": str(tmp_path), "status": "ready"},
        worktree_roots=[tmp_path],
        claim_state_fn=lambda nid: _claim("live"),
    )
    assert len(rows) == 1
    assert rows[0]["attempt_state"] == "live"
    assert rows[0]["lifecycle"] == "unconfirmed do"


def test_runtime_dead_claim_with_work_is_interrupted(tmp_path, monkeypatch):
    """AC5: an interrupted attempt has work, a dead owner, no delivery terminal."""
    monkeypatch.setattr(ra, "read_target_manifest", lambda wt: _manifest_for())
    monkeypatch.setattr(ra, "_commits_ahead", lambda wt, br: 4)
    rows = runtime_attempts(
        NODE, {"cwd": str(tmp_path), "status": "ready", "pr_number": 712},
        worktree_roots=[tmp_path],
        claim_state_fn=lambda nid: _claim("stale", pid=75742),
    )
    assert len(rows) == 1
    r = rows[0]
    assert r["attempt_state"] == "interrupted"
    assert r["claim_state"] == "stale"
    assert r["claim_pid"] == 75742
    assert r["pr_number"] == 712
    assert r["lifecycle"] == "unconfirmed do"  # never a confirmed lifecycle row


def test_runtime_dead_claim_no_work_is_stale(tmp_path, monkeypatch):
    monkeypatch.setattr(ra, "read_target_manifest", lambda wt: _manifest_for())
    monkeypatch.setattr(ra, "_commits_ahead", lambda wt, br: None)  # no work evidence
    rows = runtime_attempts(
        NODE, {"cwd": str(tmp_path), "status": "ready"},
        worktree_roots=[tmp_path],
        claim_state_fn=lambda nid: _claim("stale"),
    )
    assert rows[0]["attempt_state"] == "stale"


def test_runtime_dedup_on_fno_id_keeps_most_affirmative(tmp_path, monkeypatch):
    """AC6: two worktrees, one fno_id -> one row; rebound/live wins over interrupted."""
    wt_a, wt_b = tmp_path / "a", tmp_path / "b"
    wt_a.mkdir(); wt_b.mkdir()
    # Same fno_id in both; one has work (interrupted), the live claim makes the
    # union 'live' (claim is global, read once).
    monkeypatch.setattr(ra, "read_target_manifest", lambda wt: _manifest_for())
    monkeypatch.setattr(ra, "_commits_ahead", lambda wt, br: 4)
    rows = runtime_attempts(
        NODE, {"cwd": str(tmp_path), "status": "ready"},
        worktree_roots=[wt_a, wt_b],
        claim_state_fn=lambda nid: _claim("live"),
    )
    assert len(rows) == 1
    assert rows[0]["attempt_state"] == "live"


def test_runtime_historical_attempt_not_relabelled_live(tmp_path, monkeypatch):
    """A historical attempt (different holder) must not inherit a newer attempt's
    live claim. The node claim is singular and global; only the manifest whose
    target_claim_holder matches the live holder may read 'live'. The older
    attempt classifies from its own work evidence (interrupted), never 'live'."""
    wt_a, wt_b = tmp_path / "a", tmp_path / "b"
    wt_a.mkdir(); wt_b.mkdir()
    manifest_a = _manifest_for(sid="sid-a", fno_id="run-a")
    manifest_b = _manifest_for(sid="sid-b", fno_id="run-b")

    def _read(wt):
        return manifest_b if str(wt).endswith("b") else manifest_a

    monkeypatch.setattr(ra, "read_target_manifest", _read)
    monkeypatch.setattr(ra, "_commits_ahead", lambda wt, br: 4)  # both have work
    rows = runtime_attempts(
        NODE, {"cwd": str(tmp_path), "status": "ready"},
        worktree_roots=[wt_a, wt_b],
        claim_state_fn=lambda nid: _claim("live", holder="target-session:sid-b", pid=999),
    )
    by_run = {r["fno_id"]: r for r in rows}
    assert len(rows) == 2
    assert by_run["run-b"]["attempt_state"] == "live"
    assert by_run["run-b"]["claim_state"] == "live"
    # run-a is a historical attempt: work + non-terminal -> interrupted, NOT live.
    assert by_run["run-a"]["attempt_state"] == "interrupted"
    assert by_run["run-a"]["claim_state"] is None


def test_runtime_skips_non_matching_manifests(tmp_path, monkeypatch):
    """A manifest for a DIFFERENT node is not projected onto this node."""
    other = _manifest_for(node="x-other")
    monkeypatch.setattr(ra, "read_target_manifest", lambda wt: other)
    rows = runtime_attempts(
        NODE, {"cwd": str(tmp_path), "status": "ready"},
        worktree_roots=[tmp_path],
        claim_state_fn=lambda nid: _claim("live"),
    )
    assert rows == []


def test_runtime_terminal_node_not_interrupted(tmp_path, monkeypatch):
    """AC8: a delivered (done) node never reads as an interrupted attempt."""
    monkeypatch.setattr(ra, "read_target_manifest", lambda wt: _manifest_for())
    monkeypatch.setattr(ra, "_commits_ahead", lambda wt, br: 4)
    rows = runtime_attempts(
        NODE, {"cwd": str(tmp_path), "status": "done", "pr_number": 800},
        worktree_roots=[tmp_path],
        claim_state_fn=lambda nid: _claim("stale"),
    )
    assert rows[0]["attempt_state"] == "stale"
