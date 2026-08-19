from __future__ import annotations

import json
from datetime import datetime, timezone

from fno.agents.provider_outage import OutageEvidence, fold_provider_outages, measure_and_persist


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
