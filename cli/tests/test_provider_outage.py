from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path

import pytest

from fno.agents.provider_outage import (
    CanaryProof,
    EvidenceIdentity,
    OutageEvidence,
    RouteCandidate,
    collect_transcript_evidence,
    fold_provider_outages,
    measure_and_persist,
    run_health_canary,
    select_healthy_destination,
)


NOW = datetime(2026, 8, 18, 18, 0, tzinfo=timezone.utc).timestamp()
FUP = (
    "API Error: Request rejected (429) · [1313][Your account's current usage pattern "
    "does not comply with the Fair Usage Policy, and your request frequency has been "
    "limited. To restore access, please submit a request.]"
)


def _record(row_id, at, content, *, status, kind="api_error", role="assistant",
            provider="zai", account="acct-a", source="transcript", pane_id=None,
            persisted=True, snapshot_at=None):
    return OutageEvidence(
        source=source, observed_at=at, row_id=row_id, harness="claude",
        provider=provider, account=account, role=role, raw_status=status,
        raw_kind=kind, content=content, pane_id=pane_id, persisted=persisted,
        snapshot_at=snapshot_at,
    )


def _fold(records, prior=None):
    return fold_provider_outages(records, prior_state=prior, now_s=NOW)


def test_ac2_429_fup_without_reset_is_terminal_but_one_session_is_not_quorum():
    report, state = _fold([_record("row-1", NOW - 30, FUP, status=429)])
    assert report["instrument"] == "measured"
    assert report["breakers"] == []
    assert report["sessions"]["row-1"] == {
        "state": "terminal", "kind": "fair_usage_policy", "consecutive": 1,
        "reset_at": None, "manual_restoration": True,
    }
    assert state["fingerprints"]


def test_ac2_429_two_distinct_rows_open_one_manual_restore_breaker():
    report, _ = _fold([
        _record("row-1", NOW - 60, FUP, status=429),
        _record("row-2", NOW - 10, FUP, status=429),
    ])
    assert len(report["breakers"]) == 1
    breaker = report["breakers"][0]
    assert (breaker["provider"], breaker["account"]) == ("zai", "acct-a")
    assert breaker["row_ids"] == ["row-1", "row-2"]
    assert breaker["reset_at"] is None and breaker["manual_restoration"] is True
    assert "positive quorum=2" in breaker["basis"]


def test_ac2_429_same_row_and_repeated_fingerprint_do_not_add_votes_or_epoch():
    first, state = _fold([
        _record("row-1", NOW - 30, FUP, status=429),
        _record("row-1", NOW - 20, FUP, status=429),
        _record("row-2", NOW - 10, FUP, status=429),
    ])
    epoch = first["breakers"][0]["outage_epoch"]
    repeated, state2 = _fold([
        _record("row-1", NOW - 30, FUP, status=429),
        _record("row-2", NOW - 10, FUP, status=429),
    ], prior=state)
    assert repeated["breakers"][0]["outage_epoch"] == epoch
    assert state2["fingerprints"] == state["fingerprints"]


def test_ac1_det_groups_only_the_same_explicit_provider_and_account():
    report, _ = _fold([
        _record("row-1", NOW - 30, FUP, status=429),
        _record("row-2", NOW - 20, FUP, status=429, account="acct-b"),
        _record("row-3", NOW - 10, FUP, status=429, provider="vendor-c"),
    ])
    assert report["breakers"] == []
    assert report["counts"]["terminal"] == 3


def test_ac1_det_real_claude_fup_record_is_collected_from_raw_transcript(tmp_path):
    transcript = tmp_path / "session.jsonl"
    transcript.write_text(json.dumps({
        "type": "assistant",
        "timestamp": "2026-08-18T17:59:30Z",
        "isApiErrorMessage": True,
        "apiErrorStatus": 429,
        "message": {"role": "assistant", "content": [{"type": "text", "text": FUP}]},
    }) + "\n", encoding="utf-8")
    identity = EvidenceIdentity(
        row_id="row-1", harness="claude", provider="anthropic",
        account="acct-a", session_id="session-1", cwd=str(tmp_path),
    )

    records, refusals = collect_transcript_evidence(
        [identity], now_s=NOW,
        transcript_path_for=lambda _identity: transcript,
    )

    assert refusals == []
    assert len(records) == 1
    assert records[0].raw_status == 429
    assert records[0].raw_kind == "api_error"
    assert records[0].role == "assistant"
    assert "Fair Usage Policy" in records[0].content


def test_ac1_det_quoted_user_record_and_missing_route_identity_do_not_vote(tmp_path):
    transcript = tmp_path / "session.jsonl"
    transcript.write_text("\n".join([
        json.dumps({
            "type": "user", "timestamp": "2026-08-18T17:59:20Z",
            "isApiErrorMessage": True, "apiErrorStatus": 429,
            "message": {"role": "user", "content": FUP},
        }),
        json.dumps({
            "type": "assistant", "timestamp": "2026-08-18T17:59:30Z",
            "isApiErrorMessage": True, "apiErrorStatus": 429,
            "message": {"role": "assistant", "content": FUP},
        }),
    ]) + "\n", encoding="utf-8")
    identities = [
        EvidenceIdentity("quoted", "claude", "anthropic", "acct-a", "s1", str(tmp_path)),
        EvidenceIdentity("unknown", "claude", None, "acct-b", "s2", str(tmp_path)),
    ]

    records, refusals = collect_transcript_evidence(
        identities, now_s=NOW,
        transcript_path_for=lambda _identity: transcript,
    )

    assert [record.row_id for record in records] == ["quoted"]
    assert all(record.role == "assistant" for record in records)
    assert refusals == [{
        "row_id": "unknown", "reason": "unknown_route_identity", "count": 1,
    }]


def test_ac1_det_rejects_user_quotes_and_unknown_route_identity():
    report, _ = _fold([
        _record("row-1", NOW - 30, FUP, status=429, role="user"),
        _record("row-2", NOW - 20, FUP, status=429, provider=None),
        _record("row-3", NOW - 10, FUP, status=429, account=None),
    ])
    assert report["breakers"] == [] and report["counts"]["refused"] == 3
    assert {item["reason"] for item in report["refusals"]} == {
        "not_assistant_api_record", "unknown_route_identity"
    }


def test_ac1_det_stale_or_unpersisted_pane_evidence_is_refused():
    report, _ = _fold([
        _record("row-1", NOW - 10, FUP, status=429, source="pane",
                pane_id="pane-1", persisted=False, snapshot_at=NOW - 10),
        _record("row-2", NOW - 300, FUP, status=429, source="pane",
                pane_id="pane-2", persisted=True, snapshot_at=NOW - 300),
    ])
    assert report["breakers"] == [] and report["counts"]["refused"] == 2
    assert {item["reason"] for item in report["refusals"]} == {
        "pane_not_persisted", "pane_snapshot_stale"
    }


def test_ac4_err_unreadable_journal_is_unknown_and_does_not_act(tmp_path):
    journal = tmp_path / "recovery" / "provider-outages.json"
    journal.parent.mkdir()
    journal.write_text("{not-json", encoding="utf-8")
    report = measure_and_persist([
        _record("row-1", NOW - 20, FUP, status=429),
        _record("row-2", NOW - 10, FUP, status=429),
    ], now_s=NOW, path=journal)
    assert report["instrument"] == "unknown" and report["breakers"] == []
    assert report["counts"] == {"journal_unreadable": 1}
    assert report["refusals"][0]["reason"] == "journal_unreadable"
    assert journal.read_text(encoding="utf-8") == "{not-json"


def test_ac3_529_retries_until_three_consecutive_records_span_two_minutes():
    records = [
        _record("row-1", NOW - 100, "API Error: 529 Overloaded", status=529),
        _record("row-1", NOW - 50, "API Error: 529 Overloaded", status=529),
        _record("row-1", NOW - 1, "API Error: 529 Overloaded", status=529),
    ]
    report, _ = _fold(records)
    assert report["sessions"]["row-1"]["state"] == "retrying"
    records[0] = _record("row-1", NOW - 121, "API Error: 529 Overloaded", status=529)
    report, _ = _fold(records)
    assert report["sessions"]["row-1"]["state"] == "session_persistent"
    assert report["breakers"] == []


def test_ac3_529_successful_assistant_content_resets_the_sequence():
    report, _ = _fold([
        _record("row-1", NOW - 240, "API Error: 529 Overloaded", status=529),
        _record("row-1", NOW - 180, "API Error: 529 Overloaded", status=529),
        _record("row-1", NOW - 120, "request succeeded", status=None, kind="content"),
        _record("row-1", NOW - 60, "API Error: 529 Overloaded", status=529),
        _record("row-1", NOW - 1, "API Error: 529 Overloaded", status=529),
    ])
    assert report["sessions"]["row-1"]["state"] == "retrying"
    assert report["sessions"]["row-1"]["consecutive"] == 2


def test_ac3_529_unpersisted_pane_content_cannot_reset_the_sequence():
    report, _ = _fold([
        _record("row-1", NOW - 240, "API Error: 529 Overloaded", status=529),
        _record("row-1", NOW - 180, "API Error: 529 Overloaded", status=529),
        _record(
            "row-1", NOW - 120, "request succeeded", status=None, kind="content",
            source="pane", pane_id="pane-1", persisted=False, snapshot_at=NOW - 120,
        ),
        _record("row-1", NOW - 60, "API Error: 529 Overloaded", status=529),
        _record("row-1", NOW - 1, "API Error: 529 Overloaded", status=529),
    ])
    assert report["sessions"]["row-1"]["state"] == "session_persistent"
    assert report["counts"]["refused"] == 1


def test_ac3_529_two_persistent_sessions_open_breaker():
    records = []
    for row_id, offset in (("row-1", 0), ("row-2", 20)):
        records += [
            _record(row_id, NOW - 180 + offset, "API Error: 529 Overloaded", status=529),
            _record(row_id, NOW - 120 + offset, "API Error: 529 Overloaded", status=529),
            _record(row_id, NOW - 60 + offset, "API Error: 529 Overloaded", status=529),
        ]
    report, _ = _fold(records)
    assert len(report["breakers"]) == 1
    assert report["breakers"][0]["kind"] == "overloaded_529"
    assert "positive quorum=2" in report["breakers"][0]["basis"]


def test_ac1_det_persisted_pane_snapshot_is_durable_before_it_votes(tmp_path):
    journal = tmp_path / "provider-outages.json"
    report = measure_and_persist([
        _record("row-1", NOW - 20, FUP, status=429, source="pane",
                pane_id="pane-1", persisted=True, snapshot_at=NOW - 20),
        _record("row-2", NOW - 10, FUP, status=429),
    ], now_s=NOW, path=journal)
    assert len(report["breakers"]) == 1
    stored = json.loads(journal.read_text(encoding="utf-8"))
    assert stored["pane_snapshots"][0]["pane_id"] == "pane-1"
    assert stored["pane_snapshots"][0]["fingerprint"]


def test_ac5_hlth_route_selection_excludes_every_candidate_on_broken_provider():
    proof = CanaryProof(
        source="transcript", content="FNO_PROVIDER_HEALTH_OK", observed_at=NOW - 1,
        persisted=True, assistant_role=True,
    )
    candidates = [
        RouteCandidate(
            record_id="other-account", harness="opencode", provider="zai",
            account="acct-b", account_env={}, canary=proof,
        ),
        RouteCandidate(
            record_id="openai-account", harness="codex", provider="openai",
            account="acct-c", account_env={"OPENAI_API_KEY": "test"}, canary=proof,
        ),
    ]

    selected = select_healthy_destination(
        candidates, broken_provider="zai", now_s=NOW,
    )

    assert selected == candidates[1]
    assert selected.harness == "codex"
    assert selected.provider == "openai"
    assert selected.account == "acct-c"
    assert selected.record_id == "openai-account"


def test_ac5_hlth_missing_route_identity_is_unknown_and_ineligible():
    proof = CanaryProof(
        source="transcript", content="FNO_PROVIDER_HEALTH_OK", observed_at=NOW,
        persisted=True, assistant_role=True,
    )
    candidates = [
        RouteCandidate(
            record_id="a", harness="codex", provider=None, account="acct",
            account_env={}, canary=proof,
        ),
        RouteCandidate(
            record_id="b", harness="codex", provider="openai", account=None,
            account_env={}, canary=proof,
        ),
    ]
    assert select_healthy_destination(candidates, broken_provider="zai", now_s=NOW) is None


@pytest.mark.parametrize(
    "proof",
    [
        None,
        CanaryProof(
            source="transcript", content="FNO_PROVIDER_HEALTH_OK",
            observed_at=NOW - 121, persisted=True, assistant_role=True,
        ),
        CanaryProof(
            source="transcript", content="FNO_PROVIDER_HEALTH_OK",
            observed_at=NOW - 1, persisted=True, assistant_role=False,
        ),
        CanaryProof(
            source="pane", content="prompt: FNO_PROVIDER_HEALTH_OK",
            observed_at=NOW - 1, persisted=True, assistant_role=False,
            pane_id="pane-1",
        ),
    ],
)
def test_ac5_hlth_absent_stale_or_prompt_echo_marker_is_not_health(proof):
    candidate = RouteCandidate(
        record_id="a", harness="codex", provider="openai", account="acct",
        account_env={}, canary=proof,
    )
    assert select_healthy_destination([candidate], broken_provider="zai", now_s=NOW) is None


@pytest.mark.parametrize(
    "proof",
    [
        CanaryProof(
            source="transcript", content="FNO_PROVIDER_HEALTH_OK",
            observed_at=NOW - 1, persisted=True, assistant_role=True,
        ),
        CanaryProof(
            source="pane", content="FNO_PROVIDER_HEALTH_OK", observed_at=NOW - 1,
            persisted=True, assistant_role=False, pane_id="pane-agy",
        ),
    ],
)
def test_ac5_hlth_exact_fresh_transcript_or_persisted_agy_pane_marker_is_health(proof):
    candidate = RouteCandidate(
        record_id="a", harness="agy", provider="google", account="acct",
        account_env={}, canary=proof,
    )
    assert select_healthy_destination(
        [candidate], broken_provider="zai", now_s=NOW,
    ) == candidate


def test_ac5_hlth_preserves_order_pin_and_filters_capability_runtime_and_breaker():
    proof = CanaryProof(
        source="transcript", content="FNO_PROVIDER_HEALTH_OK", observed_at=NOW,
        persisted=True, assistant_role=True,
    )

    def candidate(record_id, **changes):
        values = {
            "record_id": record_id, "harness": "codex", "provider": "openai",
            "account": record_id, "account_env": {}, "canary": proof,
        }
        values.update(changes)
        return RouteCandidate(**values)

    candidates = [
        candidate("breaker", breaker_open=True),
        candidate("runtime", runtime_exhausted=True),
        candidate("missing", harness_installed=False),
        candidate("substrate", pane_supported=False),
        candidate("full", pane_count=4),
        candidate("first"),
        candidate("second"),
    ]
    assert select_healthy_destination(
        candidates, broken_provider="zai", now_s=NOW,
    ) == candidates[5]
    assert select_healthy_destination(
        candidates, broken_provider="zai", now_s=NOW, pinned_record_id="second",
    ) == candidates[6]
    assert select_healthy_destination(
        candidates, broken_provider="zai", now_s=NOW, pinned_record_id="missing-pin",
    ) is None


def test_ac5_hlth_canary_uses_canonical_spawn_outside_node_then_stops(tmp_path: Path):
    node_cwd = tmp_path / "node"
    canary_cwd = tmp_path / "neutral"
    node_cwd.mkdir()
    canary_cwd.mkdir()
    candidate = RouteCandidate(
        record_id="a", harness="agy", provider="google", account="acct",
        account_env={"GOOGLE_API_KEY": "test"}, canary=None,
    )
    calls = []
    stopped = []
    snapshots = iter([{"node:existing"}, {"node:existing"}])

    def spawn(**kwargs):
        calls.append(kwargs)
        return {"pane_id": 7}

    proof = run_health_canary(
        candidate,
        canary_cwd=canary_cwd,
        node_cwd=node_cwd,
        now_s=NOW,
        spawn=spawn,
        collect_proof=lambda _spawned: CanaryProof(
            source="pane", content="FNO_PROVIDER_HEALTH_OK", observed_at=NOW - 1,
            persisted=True, assistant_role=False, pane_id="7", stopped=False,
        ),
        stop=lambda spawned: stopped.append(spawned) is None,
        claim_snapshot=lambda: next(snapshots),
    )

    assert proof is not None and proof.stopped is True
    assert calls[0]["provider"] == "agy"
    assert calls[0]["cwd"] == canary_cwd
    assert "node" not in calls[0] and "provenance" not in calls[0]
    assert "FNO_PROVIDER_HEALTH_OK" not in calls[0]["message"]
    assert stopped == [{"pane_id": 7}]


def test_ac5_hlth_canary_refuses_failed_stop_new_claim_or_node_worktree(tmp_path: Path):
    candidate = RouteCandidate(
        record_id="a", harness="codex", provider="openai", account="acct",
        account_env={}, canary=None,
    )
    fresh = CanaryProof(
        source="transcript", content="FNO_PROVIDER_HEALTH_OK", observed_at=NOW,
        persisted=True, assistant_role=True,
    )
    def spawn(**_kwargs):
        return object()
    assert run_health_canary(
        candidate, canary_cwd=tmp_path, node_cwd=tmp_path / "node", now_s=NOW,
        spawn=spawn, collect_proof=lambda _spawned: fresh, stop=lambda _spawned: False,
        claim_snapshot=lambda: set(),
    ) is None
    snapshots = iter([set(), {"node:new"}])
    assert run_health_canary(
        candidate, canary_cwd=tmp_path, node_cwd=tmp_path / "node", now_s=NOW,
        spawn=spawn, collect_proof=lambda _spawned: fresh, stop=lambda _spawned: True,
        claim_snapshot=lambda: next(snapshots),
    ) is None
    assert run_health_canary(
        candidate, canary_cwd=tmp_path / "node" / "inside", node_cwd=tmp_path / "node",
        now_s=NOW, spawn=spawn, collect_proof=lambda _spawned: fresh,
        stop=lambda _spawned: True, claim_snapshot=lambda: set(),
    ) is None
