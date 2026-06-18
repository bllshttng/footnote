"""Unit tests for fno.graph.statuses - recompute_statuses and is_stale_lock."""
from __future__ import annotations

from datetime import datetime, timezone, timedelta

import pytest

from fno.graph.statuses import recompute_statuses, is_stale_lock


def _entry(eid: str, **kwargs) -> dict:
    base = {
        "id": eid,
        "title": eid,
        "completed_at": None,
        "session_id": None,
        "claimed_at": None,
        "blocked_by": [],
        # A stub plan_path so the default fixture exercises the ready/blocked/
        # claimed/done branches; tests for the idea derivation explicitly omit
        # plan_path or set it to None.
        "plan_path": f"plans/{eid}.md",
        "_status": "ready",
    }
    base.update(kwargs)
    return base


# -- is_stale_lock --

def test_ac1_hp_is_stale_lock_no_session():
    """AC1-HP: entry without session_id is not stale."""
    e = _entry("ab-11111111")
    assert is_stale_lock(e) is False


def test_ac1_hp_is_stale_lock_fresh_claim():
    """AC1-HP: recently claimed entry is not stale."""
    now = datetime.now(timezone.utc).isoformat()
    e = _entry("ab-22222222", session_id="sess-001", claimed_at=now)
    assert is_stale_lock(e) is False


def test_ac1_hp_is_stale_lock_old_claim():
    """AC1-HP: claim older than TTL is stale."""
    old = (datetime.now(timezone.utc) - timedelta(hours=3)).isoformat()
    e = _entry("ab-33333333", session_id="sess-001", claimed_at=old)
    assert is_stale_lock(e) is True


def test_ac2_err_is_stale_lock_bad_timestamp():
    """AC2-ERR: unparseable timestamp is treated as stale."""
    e = _entry("ab-44444444", session_id="sess-001", claimed_at="not-a-date")
    assert is_stale_lock(e) is True


# -- recompute_statuses --

def test_ac1_hp_recompute_ready():
    """AC1-HP: entry with no blockers and no session is ready."""
    entries = [_entry("ab-aaaaaaaa")]
    result = recompute_statuses(entries)
    assert result[0]["_status"] == "ready"


def test_ac1_hp_recompute_done():
    """AC1-HP: entry with completed_at is done."""
    entries = [_entry("ab-bbbbbbbb", completed_at="2026-01-01T00:00:00Z")]
    result = recompute_statuses(entries)
    assert result[0]["_status"] == "done"


def test_ac1_hp_recompute_blocked():
    """AC1-HP: entry blocked by incomplete node is blocked."""
    entries = [
        _entry("ab-cccccccc"),
        _entry("ab-dddddddd", blocked_by=["ab-cccccccc"]),
    ]
    result = recompute_statuses(entries)
    statuses = {e["id"]: e["_status"] for e in result}
    assert statuses["ab-cccccccc"] == "ready"
    assert statuses["ab-dddddddd"] == "blocked"


def test_ac1_hp_recompute_unblock_on_completion():
    """AC1-HP: completing a blocker unblocks the dependent."""
    entries = [
        _entry("ab-eeeeeeee", completed_at="2026-01-01T00:00:00Z"),
        _entry("ab-ffffffff", blocked_by=["ab-eeeeeeee"]),
    ]
    result = recompute_statuses(entries)
    statuses = {e["id"]: e["_status"] for e in result}
    assert statuses["ab-eeeeeeee"] == "done"
    assert statuses["ab-ffffffff"] == "ready"


def test_ac1_hp_recompute_claimed():
    """AC1-HP: entry with active session_id is claimed."""
    now = datetime.now(timezone.utc).isoformat()
    entries = [_entry("ab-gggggggg", session_id="sess-active", claimed_at=now)]
    result = recompute_statuses(entries)
    assert result[0]["_status"] == "claimed"


def test_ac1_hp_recompute_stale_lock_cleared():
    """AC1-HP: stale lock is cleared and entry reverts to ready."""
    old = (datetime.now(timezone.utc) - timedelta(hours=5)).isoformat()
    entries = [_entry("ab-hhhhhhhh", session_id="old-sess", claimed_at=old)]
    result = recompute_statuses(entries)
    e = result[0]
    assert e["_status"] == "ready"
    assert e["session_id"] is None
    assert e["claimed_at"] is None


def test_ac1_hp_recompute_cascade_unblock():
    """AC1-HP: chain A->B->C: completing A unblocks B but C stays blocked."""
    entries = [
        _entry("ab-aaaabbbb", completed_at="2026-01-01T00:00:00Z"),
        _entry("ab-bbbbcccc", blocked_by=["ab-aaaabbbb"]),
        _entry("ab-ccccdddd", blocked_by=["ab-bbbbcccc"]),
    ]
    result = recompute_statuses(entries)
    statuses = {e["id"]: e["_status"] for e in result}
    assert statuses["ab-aaaabbbb"] == "done"
    assert statuses["ab-bbbbcccc"] == "ready"
    assert statuses["ab-ccccdddd"] == "blocked"


# -- recompute_statuses idea state --


def test_ac1_hp_recompute_idea_when_plan_path_none():
    """AC1-HP: a plan-less node derives to idea when otherwise-ready."""
    e = _entry("ab-ideaa001", plan_path=None)
    result = recompute_statuses([e])
    assert result[0]["_status"] == "idea"


def test_ac1_hp_recompute_idea_when_plan_path_missing():
    """AC1-HP: an entry without a plan_path key at all also derives to idea.

    Future-proofs against historical graph.json rows that may not carry the
    field. ``recompute_statuses`` reads via ``.get`` so missing == None.
    """
    e = _entry("ab-ideaa002")
    e.pop("plan_path", None)
    result = recompute_statuses([e])
    assert result[0]["_status"] == "idea"


def test_ac4_edge_idea_overridden_by_claimed():
    """AC4-EDGE: a plan-less but actively-claimed node stays claimed."""
    now = datetime.now(timezone.utc).isoformat()
    e = _entry(
        "ab-ideaa003",
        plan_path=None,
        session_id="sess-active",
        claimed_at=now,
    )
    result = recompute_statuses([e])
    assert result[0]["_status"] == "claimed"


def test_ac4_edge_idea_overridden_by_blocked():
    """AC4-EDGE: a plan-less node with an open blocker stays blocked."""
    entries = [
        _entry("ab-blockero", plan_path=None),
        _entry("ab-ideaa004", plan_path=None, blocked_by=["ab-blockero"]),
    ]
    result = recompute_statuses(entries)
    statuses = {e["id"]: e["_status"] for e in result}
    assert statuses["ab-blockero"] == "idea"  # the blocker itself is also plan-less
    assert statuses["ab-ideaa004"] == "blocked"


def test_ac4_edge_node_with_plan_path_remains_ready():
    """AC4-EDGE: a node with a plan_path resolves to ready, never idea."""
    e = _entry("ab-readyy01", plan_path="plans/some-plan.md")
    result = recompute_statuses([e])
    assert result[0]["_status"] == "ready"


def test_ac4_edge_idea_when_plan_path_is_empty_string():
    """AC4-EDGE: empty-string plan_path is treated as no-plan, deriving idea.

    Defensive: matches the falsy check in `triage._read_plan_excerpt` so a
    row that was assigned `plan_path: ""` somewhere doesn't slip past the
    cascade as ready.
    """
    e = _entry("ab-emptyplan", plan_path="")
    result = recompute_statuses([e])
    assert result[0]["_status"] == "idea"
