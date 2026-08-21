"""Tests for durable typed resume receipts (x-c3a2).

Covers the two invariants the module exists to enforce:
  - a receipt is evidence, not authority (write is a snapshot; revalidate is
    the fail-closed gate and never mutates predecessor state/claims);
  - no resume database (event canonicalization reuses the scoreboard reducer's
    dedup + latest-by-parsed-timestamp rule, so duplicated/out-of-order events
    across the global + delivery-root journals fold to one observation).

Plus every fail-closed revalidation case named in the plan: malformed, stale
HEAD, foreign claim, dead worktree, duplicate generation, superseded-by-later.
"""
from __future__ import annotations

import json

import pytest

from fno.resume.receipt import (
    MalformedReceiptError,
    RECEIPT_VERSION,
    build_receipt,
    canonicalize_node_events,
    detect_duplicate_generation,
    latest_observation,
    load_receipt,
    revalidate,
    write_receipt,
)


# ---------------------------------------------------------------------------
# fixtures
# ---------------------------------------------------------------------------


def _receipt(**overrides):
    base = dict(
        node="x-c3a2",
        session="s1",
        phase="do",
        generation=2,
        repo="footnote",
        worktree="/wt/x-c3a2",
        branch="feature/x-c3a2",
        head="abc123def456",
        next_verb="/fno:execute waves",
        next_target="x-c3a2",
        written_at="2026-07-26T02:00:00Z",
        completed_tasks=["1.1"],
        remaining_tasks=["2.1", "2.2"],
        open_findings=["finding-1"],
        known_reds=["smoke"],
        claims=[{"key": "node:x-c3a2", "holder": "s1"}],
        watchers=["ci"],
        idempotency_keys=["pr_create:abc123d", "comment:abc123d"],
    )
    base.update(overrides)
    return build_receipt(**base)


def _ok_kwargs(**overrides):
    base = dict(
        live_head="abc123def456",
        live_branch="feature/x-c3a2",
        worktree_exists=True,
        live_claim_holder="s1",
        own_session="s1",
        node_events=[],
        harness="claude",
    )
    base.update(overrides)
    return base


# ---------------------------------------------------------------------------
# build + identity
# ---------------------------------------------------------------------------


def test_build_stamps_version_and_candidate_sha():
    r = _receipt()
    assert r.version == RECEIPT_VERSION
    # candidate_sha is the short HEAD; identity versions per (phase, gen, HEAD)
    assert r.identity.candidate_sha == "abc123def456"
    assert r.content_sha  # computed integrity tag


def test_build_rejects_bad_identity():
    with pytest.raises(MalformedReceiptError):
        build_receipt(
            node="x-c3a2", session="", phase="do", generation=2, repo="r",
            worktree="/w", branch="b", head="h", next_verb="v",
            next_target=None, written_at="2026-07-26T02:00:00Z",
        )
    with pytest.raises(MalformedReceiptError):
        build_receipt(
            node="x-c3a2", session="s", phase="do", generation=0, repo="r",
            worktree="/w", branch="b", head="h", next_verb="v",
            next_target=None, written_at="2026-07-26T02:00:00Z",
        )
    with pytest.raises(MalformedReceiptError):
        build_receipt(
            node="x-c3a2", session="s", phase="do", generation=2, repo="r",
            worktree="/w", branch="b", head="h", next_verb="  ",
            next_target=None, written_at="2026-07-26T02:00:00Z",
        )


# ---------------------------------------------------------------------------
# write + load + immutability
# ---------------------------------------------------------------------------


def test_write_and_load_round_trip(tmp_path):
    r = _receipt()
    p = write_receipt(r, tmp_path / "artifacts")
    assert p.name == "receipt-x-c3a2-do-g2-abc123def456.json"
    loaded = load_receipt(p)
    assert loaded.identity == r.identity
    assert loaded.head == r.head
    assert loaded.remaining_tasks == ("2.1", "2.2")
    assert loaded.idempotency_keys == ("pr_create:abc123d", "comment:abc123d")
    assert loaded.next_action.verb == "/fno:execute waves"
    assert loaded.content_sha == r.content_sha


def test_write_is_immutable_refuses_overwrite(tmp_path):
    r = _receipt()
    write_receipt(r, tmp_path)
    with pytest.raises(FileExistsError):
        write_receipt(r, tmp_path)


def test_new_head_at_same_phase_is_new_version_not_overwrite(tmp_path):
    # A different HEAD at the same phase/generation is a distinct identity, so
    # it writes a new file rather than clobbering the prior version.
    write_receipt(_receipt(head="abc123def456"), tmp_path)
    p2 = write_receipt(_receipt(head="ffffff000000"), tmp_path)
    assert p2.exists()
    assert (tmp_path / "receipt-x-c3a2-do-g2-abc123def456.json").exists()
    assert (tmp_path / "receipt-x-c3a2-do-g2-ffffff000000.json").exists()


# ---------------------------------------------------------------------------
# malformed receipt -> fail closed (never read as empty evidence)
# ---------------------------------------------------------------------------


def _corrupt_json(_d):
    return "{not json"


def _drop_identity(d):
    d.pop("identity", None)


def _empty_node(d):
    d["identity"]["node"] = ""


def _gen_string(d):
    d["identity"]["generation"] = "two"


def _gen_bool(d):
    d["identity"]["generation"] = True


def _drop_next_action(d):
    d.pop("next_action", None)


def _empty_verb(d):
    d["next_action"]["verb"] = ""


def _tasks_not_list(d):
    d["remaining_tasks"] = "2.1"


def _tasks_non_strings(d):
    d["remaining_tasks"] = [1, 2]


def _claims_not_list(d):
    d["claims"] = "not-a-list"


def _unknown_version(d):
    d["version"] = 999


@pytest.mark.parametrize(
    "mutate",
    [
        _corrupt_json,  # corrupt JSON
        _drop_identity,  # missing identity
        _empty_node,  # empty required field
        _gen_string,  # non-int generation
        _gen_bool,  # bool-as-int generation
        _drop_next_action,  # missing next_action
        _empty_verb,  # empty verb
        _tasks_not_list,  # tasks not a list
        _tasks_non_strings,  # list of non-strings
        _claims_not_list,  # claims not a list
        _unknown_version,  # unknown version
    ],
)
def test_malformed_receipt_rejected(tmp_path, mutate):
    p = write_receipt(_receipt(), tmp_path)
    data = json.loads(p.read_text())
    result = mutate(data)
    p.write_text(result if isinstance(result, str) else json.dumps(data))
    with pytest.raises(MalformedReceiptError):
        load_receipt(p)


def test_content_sha_tamper_detected(tmp_path):
    p = write_receipt(_receipt(), tmp_path)
    data = json.loads(p.read_text())
    data["remaining_tasks"] = ["sneaky-extra"]  # payload changed, sha unchanged
    p.write_text(json.dumps(data))
    with pytest.raises(MalformedReceiptError):
        load_receipt(p)


def test_missing_file_is_not_malformed(tmp_path):
    # Missing is a distinct condition (caller decides); only present-but-corrupt
    # raises MalformedReceiptError.
    with pytest.raises(FileNotFoundError):
        load_receipt(tmp_path / "absent.json")


# ---------------------------------------------------------------------------
# reducer reuse: canonicalize + latest-by-timestamp
# ---------------------------------------------------------------------------


def test_canonicalize_dedups_and_orders_by_timestamp():
    evs = [
        {"type": "loop_check", "ts": "2026-07-26T03:00:00Z", "data": {"node_id": "x-c3a2", "ci": "FAILURE:x"}},
        {"type": "loop_check", "ts": "2026-07-26T01:00:00Z", "data": {"node_id": "x-c3a2", "ci": "SUCCESS"}},
        {"type": "loop_check", "ts": "2026-07-26T01:00:00Z", "data": {"node_id": "x-c3a2", "ci": "SUCCESS"}},  # dup
    ]
    canon = canonicalize_node_events(evs)
    assert len(canon) == 2  # dedup by signature
    assert [c["data"]["ci"] for c in canon] == ["SUCCESS", "FAILURE:x"]  # ts order


def test_latest_observation_kind_filtered():
    evs = [
        {"type": "loop_check", "ts": "2026-07-26T01:00:00Z", "data": {"node_id": "x-c3a2"}},
        {"type": "termination", "ts": "2026-07-26T02:00:00Z", "data": {"node_id": "x-c3a2"}},
    ]
    latest_all = latest_observation(evs)
    assert latest_all["type"] == "termination"
    latest_lc = latest_observation(evs, kinds={"loop_check"})
    assert latest_lc["type"] == "loop_check"


# ---------------------------------------------------------------------------
# duplicate generation
# ---------------------------------------------------------------------------


def _delegated(gen, frm, harness="claude"):
    return {
        "type": "delegated",
        "ts": "2026-07-26T02:30:00Z",
        "data": {"node_id": "x-c3a2", "harness": harness, "generation": gen, "from_session": frm},
    }


def test_duplicate_generation_foreign_chain_detected():
    evs = [_delegated(2, "OTHER-SESSION")]
    assert detect_duplicate_generation(
        evs, node="x-c3a2", harness="claude", generation=2, own_session="s1"
    )


def test_own_session_delegation_is_not_duplicate():
    evs = [_delegated(2, "s1")]
    assert not detect_duplicate_generation(
        evs, node="x-c3a2", harness="claude", generation=2, own_session="s1"
    )


def test_duplicate_generation_scoped_to_harness():
    # A codex-lineage delegation does not consume a claude generation slot.
    evs = [_delegated(2, "codex-session", harness="codex")]
    assert not detect_duplicate_generation(
        evs, node="x-c3a2", harness="claude", generation=2, own_session="s1"
    )


def test_duplicate_generation_malformed_gen_skipped():
    evs = [
        {"type": "delegated", "ts": "2026-07-26T02:30:00Z",
         "data": {"node_id": "x-c3a2", "harness": "claude", "generation": "oops", "from_session": "OTHER"}},
    ]
    assert not detect_duplicate_generation(
        evs, node="x-c3a2", harness="claude", generation=2, own_session="s1"
    )


# ---------------------------------------------------------------------------
# revalidate: ok + every fail-closed case
# ---------------------------------------------------------------------------


def test_revalidate_ok():
    assert revalidate(_receipt(), **_ok_kwargs()).ok is True


def test_revalidate_free_claim_is_ok_successor_acquires():
    # No live holder: the successor acquires via the canonical primitive. A free
    # claim is not foreign; it is safe continuation.
    res = revalidate(_receipt(), **_ok_kwargs(live_claim_holder=None))
    assert res.ok is True


def test_revalidate_own_session_holder_ok():
    # Predecessor (own session) still holds: idempotent re-acquire path.
    res = revalidate(_receipt(), **_ok_kwargs(live_claim_holder="s1", own_session="s1"))
    assert res.ok is True


def test_revalidate_stale_head():
    res = revalidate(_receipt(), **_ok_kwargs(live_head="zzzzzzz"))
    assert res.ok is False and res.reason == "stale_head"


def test_revalidate_stale_branch():
    res = revalidate(_receipt(), **_ok_kwargs(live_branch="feature/other"))
    assert res.ok is False and res.reason == "stale_branch"


def test_revalidate_dead_worktree():
    res = revalidate(_receipt(), **_ok_kwargs(worktree_exists=False))
    assert res.ok is False and res.reason == "dead_worktree"


def test_revalidate_foreign_claim():
    res = revalidate(_receipt(), **_ok_kwargs(live_claim_holder="other-session"))
    assert res.ok is False and res.reason == "foreign_claim"


def test_revalidate_duplicate_generation():
    res = revalidate(
        _receipt(),
        **_ok_kwargs(node_events=[_delegated(2, "OTHER-SESSION")]),
    )
    assert res.ok is False and res.reason == "duplicate_generation"


def test_revalidate_superseded_by_later_event():
    later_termination = [
        {"type": "termination", "ts": "2026-07-26T05:00:00Z", "data": {"node_id": "x-c3a2"}},
    ]
    res = revalidate(_receipt(), **_ok_kwargs(node_events=later_termination))
    assert res.ok is False
    assert res.reason.startswith("superseded_by_later_event")


def test_revalidate_undated_late_event_does_not_falsely_fail():
    # An undated event has no parsed ts; it must not false-positive a live
    # receipt into a stale-authority failure.
    undated = [{"type": "termination", "data": {"node_id": "x-c3a2"}}]
    res = revalidate(_receipt(), **_ok_kwargs(node_events=undated))
    assert res.ok is True


def test_revalidate_own_delegation_does_not_supersede():
    # handoff.sh writes the receipt (Step 2) then emits its OWN `delegated` event
    # (Step 8); that own continuation must not supersede the receipt it produced.
    # Only a FOREIGN later event is stale authority reviving.
    own_delegation = [
        {"type": "delegated", "ts": "2031-12-31T23:59:59Z",
         "data": {"node_id": "x-c3a2", "from_session": "s1", "generation": 2, "harness": "claude"}},
    ]
    res = revalidate(_receipt(), **_ok_kwargs(node_events=own_delegation))
    assert res.ok is True


def test_revalidate_foreign_later_delegation_supersedes():
    # A FOREIGN session's later delegation for the node is stale authority.
    res = revalidate(_receipt(), **_ok_kwargs(node_events=[
        {"type": "delegated", "ts": "2031-12-31T23:59:59Z",
         "data": {"node_id": "x-c3a2", "from_session": "other-session", "generation": 9, "harness": "claude"}},
    ]))
    assert res.ok is False
    assert res.reason.startswith("superseded_by_later_event")


def test_revalidate_is_read_only_does_not_mutate_inputs():
    # revalidate must preserve predecessor state: it neither acquires/releases
    # claims nor writes. Asserting it takes no repo/claims-root and returns a
    # frozen result (no side channels) documents the contract.
    r = _receipt()
    events = [_delegated(2, "OTHER-SESSION")]
    events_before = [dict(e) for e in events]
    res = revalidate(r, **_ok_kwargs(node_events=events))
    assert res.ok is False  # duplicate generation
    assert events == events_before  # input events untouched


def test_revalidate_checked_is_populated_for_observability():
    res = revalidate(_receipt(), **_ok_kwargs())
    assert res.checked["node"] == "x-c3a2"
    assert res.checked["live_head"] == "abc123def456"
