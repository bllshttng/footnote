"""The done=merged invariant, asserted about the graph (x-c975 US5).

This survived a prior hardening pass because the rule lived inside one close
function rather than as a statement about the data, and a guard in `cmd_done`
cannot see a writer that never calls `cmd_done`. These tests pin the property,
not any one door.
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone

import pytest

from fno.graph import triage
from fno.health_monitor import evaluate_thresholds


def _recent(days_ago: float = 1) -> str:
    return (datetime.now(timezone.utc) - timedelta(days=days_ago)).isoformat()


_UNSET = object()


def _node(nid, pr=999, completed_at=_UNSET, url="https://github.com/o/r/pull/999", **kw):
    return {
        "id": nid, "title": f"node {nid}", "pr_number": pr, "pr_url": url,
        "completed_at": _recent() if completed_at is _UNSET else completed_at,
        **kw,
    }


@pytest.fixture
def gh(monkeypatch):
    """Stub the bulk gh read. `states` maps pr_number -> state; `outages` lists repos."""
    box = {"states": {}, "outages": []}

    def _fake(pr_refs, *, limit=200):
        repos = {repo for repo, _ in pr_refs if repo}
        if box["outages"]:
            return {}, sorted(repos)
        return (
            {(repo, pr): box["states"][pr] for repo, pr in pr_refs if pr in box["states"]},
            [],
        )

    monkeypatch.setattr(triage, "_pr_states_by_repo", _fake)
    monkeypatch.setattr(triage, "_forced_close_receipts", lambda *a, **k: box.get("forced", set()))
    return box


def test_catches_a_node_closed_around_the_gate(gh):
    gh["states"] = {999: "OPEN"}
    report = triage.done_not_merged_report([_node("x-bad0")])

    assert [v["id"] for v in report["violations"]] == ["x-bad0"]
    assert report["violations"][0]["pr_state"] == "OPEN"
    assert evaluate_thresholds({"done_not_merged": report["violations"]})


def test_a_merged_pr_is_clean(gh):
    gh["states"] = {999: "MERGED"}
    report = triage.done_not_merged_report([_node("x-ok00")])

    assert report["violations"] == []
    assert report["checked"] == 1


def test_gh_outage_is_unknown_not_a_violation(gh):
    """One network blip reporting a dozen false violations trains the operator to
    mute the check, which is worse than not having it."""
    gh["outages"] = ["o/r"]
    report = triage.done_not_merged_report([_node("x-out0"), _node("x-out1")])

    assert report["violations"] == []
    assert {u["id"] for u in report["unknown"]} == {"x-out0", "x-out1"}
    assert not evaluate_thresholds({"done_not_merged": report["violations"]})


def test_pre_gate_historical_closes_are_out_of_window(gh):
    """The six nodes closed before this gate existed carry merge_status: null and
    merged PRs. The window keeps them out rather than needing a discriminator."""
    gh["states"] = {999: "OPEN"}
    report = triage.done_not_merged_report(
        [_node("x-old0", completed_at=_recent(days_ago=90))]
    )

    assert report["violations"] == []
    assert report["checked"] == 0


def test_a_forced_close_carries_its_own_receipt(gh):
    """`done --force --reason` is the documented bypass and the one close that
    leaves a reason on the record."""
    gh["states"] = {999: "CLOSED"}
    gh["forced"] = {("x-frc0", 999)}
    report = triage.done_not_merged_report([_node("x-frc0")])

    assert report["violations"] == []


def test_pr_less_nodes_are_exempt(gh):
    """Advisory and doc deliverables legitimately have no PR; scoping to
    pr_number keeps them from reading red forever."""
    report = triage.done_not_merged_report(
        [{"id": "x-doc0", "title": "brief", "completed_at": _recent(), "domain": "research"}]
    )

    assert report["violations"] == []
    assert report["checked"] == 0


def test_a_url_less_pr_number_is_unknown_not_a_violation(gh):
    """A node carrying pr_number with no pr_url says nothing about which repo to
    ask, so it cannot be judged either way."""
    report = triage.done_not_merged_report([_node("x-nourl", url="")])

    assert report["violations"] == []
    assert report["unknown"][0]["reason"] == "no pr_url to resolve the repo"


def test_an_open_node_is_not_yet_subject_to_the_invariant(gh):
    """The invariant is about completed_at, the stored field - a node with a PR
    but no close is simply in flight."""
    gh["states"] = {999: "OPEN"}
    report = triage.done_not_merged_report([_node("x-live", completed_at=None)])

    assert report["violations"] == []
    assert report["checked"] == 0


def test_a_forced_close_receipt_is_read_from_the_canonical_envelope(tmp_path, monkeypatch):
    """backlog_done_forced puts node_id under `data`. Reading only the top level
    or `payload` recognizes no real receipt, so every documented bypass would
    report as a violation."""
    from fno import events as ev

    events_file = tmp_path / "events.jsonl"
    events_file.write_text(
        json.dumps(ev.backlog_done_forced(node_id="x-frc9", force_reason="abandoned spike"))
        + "\n"
    )
    monkeypatch.setattr(triage, "_events_path", lambda: events_file)

    assert ("x-frc9", None) in triage._forced_close_receipts()


def test_a_merged_additional_pr_clears_an_open_primary(gh):
    """cmd_done closes on ANY merged ref, so a node whose primary is OPEN but
    whose additional_prs carries a MERGED PR is a valid close, not a violation."""
    gh["states"] = {999: "OPEN", 1000: "MERGED"}
    node = _node("x-multi")
    node["additional_prs"] = [{"number": 1000, "url": "https://github.com/o/r/pull/1000"}]
    report = triage.done_not_merged_report([node])

    assert report["violations"] == []
    assert report["checked"] == 1


def test_an_unreadable_additional_ref_stays_unknown_not_a_violation(gh):
    """If a merged ref could be hiding behind a ref we could not read, the node
    must not read as a violation - same never-a-false-breach rule as an outage."""
    gh["states"] = {999: "OPEN"}  # 1000 is absent from the stub -> None
    node = _node("x-partial")
    node["additional_prs"] = [{"number": 1000, "url": "https://github.com/o/r/pull/1000"}]
    report = triage.done_not_merged_report([node])

    assert report["violations"] == []
    assert [u["id"] for u in report["unknown"]] == ["x-partial"]


def test_forced_receipt_is_read_from_a_scoped_foreign_root(tmp_path, monkeypatch):
    """Under --all / a foreign --project a node's force receipt lives in ITS
    repo's events log, not the invocation repo's. Reading only the invocation
    repo would report a legitimately force-closed foreign node as a violation."""
    from fno import events as ev

    foreign_root = tmp_path / "other-repo"
    (foreign_root / ".fno").mkdir(parents=True)
    (foreign_root / ".fno" / "events.jsonl").write_text(
        json.dumps(ev.backlog_done_forced(node_id="x-foreign", force_reason="abandoned"))
        + "\n"
    )
    # The invocation repo's own events log has nothing.
    inv = tmp_path / "here" / ".fno"
    inv.mkdir(parents=True)
    (inv / "events.jsonl").write_text("")
    monkeypatch.setattr(triage, "_events_path", lambda: inv / "events.jsonl")

    assert ("x-foreign", None) not in triage._forced_close_receipts(None)
    assert ("x-foreign", None) in triage._forced_close_receipts([str(foreign_root)])


def test_a_stale_receipt_does_not_exempt_a_close_over_a_different_pr(gh):
    """The exemption is scoped to the PR the force authorized. A node reopened
    and re-closed over a NEW open PR must not be shielded by the old receipt."""
    gh["states"] = {1001: "OPEN"}
    # The receipt authorized force-closing PR 999; the node now points at 1001.
    gh["forced"] = {("x-reforce", 999)}
    node = _node("x-reforce", pr=1001, url="https://github.com/o/r/pull/1001")
    report = triage.done_not_merged_report([node])

    assert [v["id"] for v in report["violations"]] == ["x-reforce"]
