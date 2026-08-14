"""Unit tests for fno.graph.statuses - recompute_statuses and is_stale_lock."""
from __future__ import annotations

from datetime import datetime, timezone, timedelta

import pytest

from fno.graph.statuses import (
    is_stale_lock,
    live_claimed_node_ids,
    recompute_statuses,
)


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
        "status": "ready",
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


def test_live_claim_read_can_fail_open_for_display_or_raise_for_mutation(monkeypatch):
    def unavailable(*args, **kwargs):
        raise OSError("claims unavailable")

    monkeypatch.setattr("fno.claims.core.list_claims", unavailable)

    assert live_claimed_node_ids() == set()
    with pytest.raises(OSError, match="claims unavailable"):
        live_claimed_node_ids(strict=True)


# -- recompute_statuses --

def test_ac1_hp_recompute_ready():
    """AC1-HP: entry with no blockers and no session is ready."""
    entries = [_entry("ab-aaaaaaaa")]
    result = recompute_statuses(entries)
    assert result[0]["status"] == "ready"


def test_ac1_hp_recompute_done():
    """AC1-HP: entry with completed_at is done."""
    entries = [_entry("ab-bbbbbbbb", completed_at="2026-01-01T00:00:00Z")]
    result = recompute_statuses(entries)
    assert result[0]["status"] == "done"


def test_ac1_hp_recompute_ignores_blocked_by_at_write_time():
    """AC1-HP: write-time status no longer derives from blocked_by.

    Dependency readiness is a cross-node join that can go stale the instant
    a sibling changes after this write, so it is answered fresh on every
    read (compute_readiness, via store._apply_graph_defaults) instead of
    snapshotted here - see test_graph_readiness.py for the read-time
    'blocked' result on this same shape.
    """
    entries = [
        _entry("ab-cccccccc"),
        _entry("ab-dddddddd", blocked_by=["ab-cccccccc"]),
    ]
    result = recompute_statuses(entries)
    statuses = {e["id"]: e["status"] for e in result}
    assert statuses["ab-cccccccc"] == "ready"
    assert statuses["ab-dddddddd"] == "ready"


def test_ac1_hp_recompute_unblock_on_completion():
    """AC1-HP: completing a blocker unblocks the dependent.

    Passes either way now: blocked_by no longer participates in the
    write-time derivation at all, so both entries were already headed to
    their non-blocked rung-based status regardless of blocked_by's contents.
    """
    entries = [
        _entry("ab-eeeeeeee", completed_at="2026-01-01T00:00:00Z"),
        _entry("ab-ffffffff", blocked_by=["ab-eeeeeeee"]),
    ]
    result = recompute_statuses(entries)
    statuses = {e["id"]: e["status"] for e in result}
    assert statuses["ab-eeeeeeee"] == "done"
    assert statuses["ab-ffffffff"] == "ready"


def test_ac1_hp_recompute_claimed():
    """AC1-HP: entry with active session_id is claimed."""
    now = datetime.now(timezone.utc).isoformat()
    entries = [_entry("ab-gggggggg", session_id="sess-active", claimed_at=now)]
    result = recompute_statuses(entries)
    assert result[0]["status"] == "in_progress"


def test_ac1_hp_recompute_stale_lock_cleared():
    """AC1-HP: stale lock is cleared and entry reverts to ready."""
    old = (datetime.now(timezone.utc) - timedelta(hours=5)).isoformat()
    entries = [_entry("ab-hhhhhhhh", session_id="old-sess", claimed_at=old)]
    result = recompute_statuses(entries)
    e = result[0]
    assert e["status"] == "ready"
    assert e["session_id"] is None
    assert e["claimed_at"] is None


def test_ac1_hp_recompute_cascade_unblock_ignores_blocked_by_at_write_time():
    """AC1-HP: chain A->B->C at write time - only A's completion matters here.

    C's blocked_by (pointing at still-open B) plays no role in write-time
    derivation: C carries a plan_path, so it derives ready via the rung
    table exactly like B. The cascade-wide "C is actually still blocked"
    fact is answered at read time - see test_graph_readiness.py.
    """
    entries = [
        _entry("ab-aaaabbbb", completed_at="2026-01-01T00:00:00Z"),
        _entry("ab-bbbbcccc", blocked_by=["ab-aaaabbbb"]),
        _entry("ab-ccccdddd", blocked_by=["ab-bbbbcccc"]),
    ]
    result = recompute_statuses(entries)
    statuses = {e["id"]: e["status"] for e in result}
    assert statuses["ab-aaaabbbb"] == "done"
    assert statuses["ab-bbbbcccc"] == "ready"
    assert statuses["ab-ccccdddd"] == "ready"


# -- recompute_statuses idea state --


def test_ac1_hp_recompute_idea_when_plan_path_none():
    """AC1-HP: a plan-less node derives to idea when otherwise-ready."""
    e = _entry("ab-ideaa001", plan_path=None)
    result = recompute_statuses([e])
    assert result[0]["status"] == "idea"


def test_ac1_hp_recompute_idea_when_plan_path_missing():
    """AC1-HP: an entry without a plan_path key at all also derives to idea.

    Future-proofs against historical graph.json rows that may not carry the
    field. ``recompute_statuses`` reads via ``.get`` so missing == None.
    """
    e = _entry("ab-ideaa002")
    e.pop("plan_path", None)
    result = recompute_statuses([e])
    assert result[0]["status"] == "idea"


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
    assert result[0]["status"] == "in_progress"


def test_ac4_edge_idea_not_overridden_by_blocked_at_write_time():
    """AC4-EDGE: a plan-less node with an open blocker derives idea at write
    time, same as if it had no blocker - blocked_by plays no role in write-time
    derivation anymore. The read-time overlay still reports it blocked for
    callers (test_graph_readiness.py).
    """
    entries = [
        _entry("ab-blockero", plan_path=None),
        _entry("ab-ideaa004", plan_path=None, blocked_by=["ab-blockero"]),
    ]
    result = recompute_statuses(entries)
    statuses = {e["id"]: e["status"] for e in result}
    assert statuses["ab-blockero"] == "idea"  # the blocker itself is also plan-less
    assert statuses["ab-ideaa004"] == "idea"


def test_ac4_edge_node_with_plan_path_remains_ready():
    """AC4-EDGE: a node with a plan_path resolves to ready, never idea."""
    e = _entry("ab-readyy01", plan_path="plans/some-plan.md")
    result = recompute_statuses([e])
    assert result[0]["status"] == "ready"


def test_ac4_edge_idea_when_plan_path_is_empty_string():
    """AC4-EDGE: empty-string plan_path is treated as no-plan, deriving idea.

    Defensive: matches the falsy check in `triage._read_plan_excerpt` so a
    row that was assigned `plan_path: ""` somewhere doesn't slip past the
    cascade as ready.
    """
    e = _entry("ab-emptyplan", plan_path="")
    result = recompute_statuses([e])
    assert result[0]["status"] == "idea"


# -- in_review: node with an open, unmerged PR is held out of dispatch --


def test_in_review_when_pr_number_set():
    """A node carrying a pr_number (not yet merged) derives in_review."""
    e = _entry("ab-prreview1", pr_number=358)
    result = recompute_statuses([e])
    assert result[0]["status"] == "in_review"


def test_in_review_survives_stale_claim():
    """The stampede case: builder session died (stale lock) but PR is still
    open. Status must stay in_review, NOT revert to ready and re-enter the
    dispatch pool."""
    old = (datetime.now(timezone.utc) - timedelta(hours=48)).isoformat()
    e = _entry("ab-prreview2", pr_number=358, session_id="dead-sess", claimed_at=old)
    result = recompute_statuses([e])
    assert result[0]["status"] == "in_review"
    # The stale lock must still be reaped (not leaked): otherwise
    # _normalize_lock_fields re-mirrors it into session_id at done time and
    # clobbers merge-time provenance.
    assert result[0]["session_id"] is None
    assert result[0]["claimed_at"] is None
    assert result[0]["locked_by"] is None


def test_in_review_reachable_through_typed_entry():
    """The Entry.status computed field must also derive in_review, so a typed
    round-trip (model_dump) does not silently emit ready/idea/blocked."""
    from fno.graph.types import Entry

    entry = Entry(id="ab-prreview5", title="t", pr_number=358)
    assert entry.status == "in_review"


def test_done_wins_over_pr_number_on_merge():
    """Once the PR merges, completed_at is set and `done` wins over in_review."""
    e = _entry("ab-prreview3", pr_number=358, completed_at="2026-07-12T00:00:00Z")
    result = recompute_statuses([e])
    assert result[0]["status"] == "done"


def test_deferred_wins_over_pr_number():
    """An explicit defer (e.g. a no-merge run parking its own open PR) still
    surfaces as deferred, not in_review."""
    e = _entry(
        "ab-prreview4",
        pr_number=358,
        deferred_at="2026-07-12T00:00:00Z",
        deferred_reason="awaiting human merge",
    )
    result = recompute_statuses([e])
    assert result[0]["status"] == "deferred"


# ---------------------------------------------------------------------------
# Rung-derived status (x-3571 wave 1)
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "status,derived",
    [("idea", "idea"), ("stub", "idea"), ("design", "design")],
)
def test_AC2_EDGE_a_linked_pre_design_plan_never_derives_ready(
    tmp_path, status, derived
):
    """The defect this wave exists to fix - the `stub` rows fail against main.

    `stub` was in no vocabulary: `is_design_stage` read False for it and the
    final `else` assigned `ready`, so a node pointing at an unfilled decompose
    scaffold was fully dispatchable. `idea` is the same rung under a word every
    gate can see.
    """
    from fno.graph.ladder import is_dispatchable

    plan = tmp_path / f"{status}.md"
    plan.write_text(f"---\nstatus: {status}\n---\n\n# Child\n")
    e = _entry("ab-predsgn1", plan_path=str(plan))

    assert recompute_statuses([e])[0]["status"] == derived
    assert derived != "ready"
    assert is_dispatchable(e) is False


def test_a_blueprinted_plan_still_derives_ready(tmp_path):
    """The other half of AC5-HP: nothing changes for a stamped, readable plan."""
    plan = tmp_path / "r.md"
    plan.write_text("---\nstatus: ready\n---\n\n# Plan\n")
    e = _entry("ab-ready002", plan_path=str(plan))
    assert recompute_statuses([e])[0]["status"] == "ready"


def test_unresolvable_plan_path_stays_ready_not_idea():
    """A declared-but-unanchorable plan_path is "cannot tell", not "no plan".

    `resolve_plan_probe` returns None for a repo-relative path on a node with
    no `cwd`, the same value it returns when there is no plan_path at all.
    Folding those together would demote every foreign relative-path node to
    `idea` and hide it from the board.
    """
    from fno.graph.ladder import Rung, plan_rung

    e = _entry("ab-norelat1", plan_path="plans/somewhere.md")  # no cwd anchor
    assert plan_rung(e) is Rung.UNREADABLE
    assert recompute_statuses([e])[0]["status"] == "ready"


def test_no_plan_path_at_all_still_derives_idea():
    assert recompute_statuses([_entry("ab-noplan01", plan_path=None)])[0]["status"] == "idea"
    assert recompute_statuses([_entry("ab-noplan02", plan_path="")])[0]["status"] == "idea"


def test_rung_to_status_table_is_total_over_the_enum():
    """A new rung must not fall through to a KeyError inside the graph lock."""
    from fno.graph.ladder import Rung
    from fno.graph.statuses import VALID_STATUSES, _rung_to_graph_status

    table = _rung_to_graph_status()
    assert set(table) == set(Rung)
    assert set(table.values()) <= VALID_STATUSES


def test_a_plan_cannot_mark_its_own_node_done(tmp_path):
    """Graph truth for the terminals is completed_at/pr_number, not the doc.

    A plan doc stamped `done` while its node has no completed_at must not
    self-certify; it derives `ready` and the real gates decide.
    """
    plan = tmp_path / "d.md"
    plan.write_text("---\nstatus: done\n---\n")
    e = _entry("ab-selfcert", plan_path=str(plan), completed_at=None)
    assert recompute_statuses([e])[0]["status"] == "ready"
