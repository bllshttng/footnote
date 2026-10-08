"""Selection-time node-claim enforcement for the native `fno backlog next`.

A node with a LIVE `node:<id>` claim at the claims root must be excluded
from selection so a second session never picks up a node another session
is actively driving. Stale/expired/released claims must NOT exclude.

The doors run through the binary against a sandbox state root; the claims
root rides FNO_CLAIMS_ROOT. A malformed roster refuses selection; an
unreachable one degrades to the registry-only view and proceeds.

Refs: ab-fcf9cec5 (double-claim of ab-1e86b88e observed across PR #397/#398).
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone

import pytest

from fno.claims.core import acquire_claim
from fno.graph.store import read_graph_strict

from tests.goldens._door import door, make_sandbox, roster_stub


# Recent so the G1 stale-ready guard never quarantines these fixtures.
_RECENT_CREATED = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat()


def _two_ready_entries():
    return [
        {"id": "ab-aaaaaaaa", "title": "A", "status": "ready", "priority": "p2",
         "created_at": _RECENT_CREATED, "project": "p", "blocked_by": [], "plan_path": "a.md"},
        {"id": "ab-bbbbbbbb", "title": "B", "status": "ready", "priority": "p2",
         "created_at": _RECENT_CREATED, "project": "p", "blocked_by": [], "plan_path": "b.md"},
    ]


def _open(phase, sid):
    return {
        "phase": phase,
        "harness": "claude",
        "session_id": sid,
        "started_at": "2026-09-09T00:00:00Z",
    }


def _claim(node_id, root, holder="target-session:other"):
    # `root` is the claims BASE: wheel and door both append .fno/claims
    # under it (FNO_CLAIMS_ROOT carries the same base semantics).
    acquire_claim(key=f"node:{node_id}", holder=holder, ttl_ms=3_600_000,
                  root=root / "claims")


def _store_entries(root):
    # The store owns state; graph.json is a frozen export, so read-backs
    # come from store rows.
    return read_graph_strict(root / "graph.json")



# `backlog rank` retired from the Python surface: its lane pin answers
# natively in the binary now, and the two integration tests that pinned the
# python leg went with it.


def test_next_skips_live_claimed_node(tmp_path):
    """A live TTL claim on ab-aaaaaaaa makes `backlog next` pick ab-bbbbbbbb."""
    root = make_sandbox(tmp_path, _two_ready_entries())
    _claim("ab-aaaaaaaa", root)
    code, out, err = door(root, ["next", "--all"], path_prepend=roster_stub(root, []))
    assert code == 0, err
    assert json.loads(out)["id"] == "ab-bbbbbbbb"


def test_next_skips_worked_node(tmp_path):
    """Occupancy has two live sources: a claim on A and a working roster
    session on C both exclude, so the pick falls to B.

    (The python leg's scan-counting pin retired with that leg; the door
    pins the occupancy contract the scans served.)
    """
    entries = _two_ready_entries() + [
        {"id": "ab-cccccccc", "title": "C", "status": "ready", "priority": "p1",
         "created_at": _RECENT_CREATED, "project": "p", "blocked_by": [], "plan_path": "c.md",
         "sessions": [_open("execute", "session-9")]},
    ]
    root = make_sandbox(tmp_path, entries)
    stub = roster_stub(root, [{"sessionId": "session-9", "name": "c-worker",
                                "state": "working", "cwd": "/tmp"}])
    _claim("ab-aaaaaaaa", root)
    code, out, err = door(root, ["next", "--all"], path_prepend=stub)
    assert code == 0, err
    assert json.loads(out)["id"] == "ab-bbbbbbbb"


def test_next_prefers_sibling_of_live_claimed_epic(tmp_path):
    entries = [
        {"id": "ab-epic001", "title": "Active epic", "type": "epic",
         "status": "ready", "priority": "p2", "created_at": "2026-02-01",
         "project": "p", "blocked_by": []},
        {"id": "ab-epic002", "title": "Idle epic", "type": "epic",
         "status": "ready", "priority": "p2", "created_at": "2026-01-01",
         "project": "p", "blocked_by": []},
        {"id": "ab-claimed1", "title": "Claimed child", "status": "ready",
         "parent": "ab-epic001", "priority": "p2", "created_at": _RECENT_CREATED,
         "project": "p", "blocked_by": [], "plan_path": "claimed.md"},
        {"id": "ab-sibling1", "title": "Active sibling", "status": "ready",
         "parent": "ab-epic001", "priority": "p2", "created_at": _RECENT_CREATED,
         "project": "p", "blocked_by": [], "plan_path": "sibling.md"},
        {"id": "ab-idlekid1", "title": "Idle child", "status": "ready",
         "parent": "ab-epic002", "priority": "p2", "created_at": _RECENT_CREATED,
         "project": "p", "blocked_by": [], "plan_path": "idle.md"},
    ]
    root = make_sandbox(tmp_path, entries)
    _claim("ab-claimed1", root)
    code, out, err = door(root, ["next", "--all"], path_prepend=roster_stub(root, []))
    assert code == 0, err
    assert json.loads(out)["id"] == "ab-sibling1"


def test_parallel_next_draw_holds_unique_nodes(tmp_path):
    """Each serialized lane claims its pick before the next lane selects."""
    max_lanes = 3
    entries = [
        {
            "id": f"ab-0000000{i}", "title": f"Node {i}", "status": "ready",
            "priority": "p1", "created_at": _RECENT_CREATED, "project": "p",
            "blocked_by": [], "plan_path": f"{i}.md",
        }
        for i in range(1, max_lanes + 2)
    ]
    root = make_sandbox(tmp_path, entries)
    stub = roster_stub(root, [])
    selected: list[str] = []

    for lane in range(max_lanes):
        code, out, err = door(root, ["next", "--all"], path_prepend=stub)
        assert code == 0, err
        picked = json.loads(out)["id"]
        assert picked not in selected
        selected.append(picked)
        _claim(picked, root, holder=f"target-session:lane-{lane}")

    assert len(set(selected)) == max_lanes


def test_no_claims_directory_is_graceful(tmp_path):
    """Absent claims dir: selection behaves exactly as before (no crash)."""
    root = make_sandbox(tmp_path, _two_ready_entries())
    (root / "claims").rmdir()
    code, out, err = door(root, ["next", "--all"], path_prepend=roster_stub(root, []))
    assert code == 0, err
    assert json.loads(out)["id"] in {"ab-aaaaaaaa", "ab-bbbbbbbb"}



def test_next_refuses_malformed_roster_and_degrades_an_unreachable_one(tmp_path):
    """The roster verdict ladder: a malformed listing refuses selection (it
    never reads as unoccupied); an unreachable one degrades to the
    registry-only view and selection proceeds on the claims verdict."""
    entries = _two_ready_entries()
    root = make_sandbox(tmp_path, entries)
    code, out, err = door(root, ["next", "--all"], path_prepend=roster_stub(root, [42]))
    assert code == 1, out
    assert "selection refused" in err
    assert '"id"' not in out
    # same ids and statuses: the refused selection must not have written
    assert [(e["id"], e.get("status")) for e in _store_entries(root)] == [
        (e["id"], e.get("status")) for e in entries
    ]

    stub = root / "stubbin"
    (stub / "claude").write_text("#!/bin/sh\necho 'roster timeout' >&2\nexit 1\n")
    code, out, err = door(root, ["next", "--all"], path_prepend=str(stub))
    assert code == 0, err
    assert json.loads(out)["id"] in {"ab-aaaaaaaa", "ab-bbbbbbbb"}


def test_next_refuses_when_the_graph_is_unreadable(tmp_path):
    """A corrupt store refuses selection; it never selects over zero rows.

    A selection over no rows prints `null`, which `advance` reads as the
    benign `no-work` skip; corruption must never collapse to that.
    """
    root = make_sandbox(tmp_path, _two_ready_entries())
    (root / "graph.db").write_bytes(b"not a database")
    code, out, err = door(root, ["next", "--all"], path_prepend=roster_stub(root, []))
    assert code == 1
    assert "graph unreadable" in err
    assert '"id"' not in out


@pytest.mark.parametrize("command", [("next", "--all"), ("ready", "--all")])
def test_dispatch_selection_refuses_when_live_claim_state_is_unavailable(
    tmp_path, command
):
    """A claims root that exists but cannot be read is UNKNOWN state, which
    must refuse, never read as "nothing is claimed"."""
    entries = _two_ready_entries()
    root = make_sandbox(tmp_path, entries)
    (root / "claims").chmod(0o000)
    try:
        code, out, err = door(root, list(command), path_prepend=roster_stub(root, []))
        assert code == 1
        assert "live claim state is unavailable" in err
        # same ids and statuses: the refused selection must not have written
        assert [(e["id"], e.get("status")) for e in _store_entries(root)] == [
            (e["id"], e.get("status")) for e in entries
        ]
    finally:
        # A mode-000 dir left behind makes the next run's rm_rf of this tree
        # fail with Errno 66 ("Directory not empty") - no process needed.
        (root / "claims").chmod(0o700)


def test_ready_excludes_live_claimed_node(tmp_path):
    """`backlog ready` omits a live-claimed node from the listing."""
    root = make_sandbox(tmp_path, _two_ready_entries())
    _claim("ab-aaaaaaaa", root)
    code, out, err = door(root, ["ready", "--all"], path_prepend=roster_stub(root, []))
    assert code == 0, err
    ids = [e["id"] for e in json.loads(out)]
    assert "ab-aaaaaaaa" not in ids
    assert "ab-bbbbbbbb" in ids



def test_a_released_claim_does_not_block(tmp_path):
    """Only LIVE claims filter: a released claim leaves its node selectable."""
    from fno.claims.core import release_claim

    root = make_sandbox(tmp_path, _two_ready_entries())
    acquire_claim(key="node:ab-aaaaaaaa", holder="h", ttl_ms=3_600_000,
                  root=root / "claims")
    release_claim(key="node:ab-aaaaaaaa", holder="h", root=root / "claims")
    code, out, err = door(root, ["ready", "--all"], path_prepend=roster_stub(root, []))
    assert code == 0, err
    ids = [e["id"] for e in json.loads(out)]
    assert "ab-aaaaaaaa" in ids
