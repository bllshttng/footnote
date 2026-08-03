"""Tests for worktree overlap recording and recurrence reporting (x-cd4d).

Every test injects a temp ``journal`` path so nothing touches the real
machine-global ``~/.fno/events.jsonl``. The fold is exercised both through the
recording verb (real append + re-read) and directly with hand-written events
so windowing and dedup are deterministic.
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from fno.events import append_event, worktree_overlap_observed
from fno.paths import global_events_json
from fno.worktree_cli.overlaps import (
    fold_overlap_events,
    overlap_record,
    overlaps_report,
    render_overlaps_text,
)

NOW = datetime(2026, 8, 3, 12, 0, 0, tzinfo=timezone.utc)
PAYLOAD = (
    '{"observer_session_id":"obs-1","peer_session_ids":["peer-a"],'
    '"worktree_git_dir":"/r/.git/worktrees/wt1",'
    '"repository_common_dir":"/r/.git","live_window_seconds":120}'
)


def _overlap_event(
    *,
    observer: str,
    peers: list[str],
    worktree: str,
    ts: datetime,
    repo: str = "/r/.git",
) -> dict:
    """A valid overlap event with a controlled envelope ts (for window tests)."""
    event = worktree_overlap_observed(
        observer_session_id=observer,
        peer_session_ids=peers,
        repository_key=repo,
        worktree_key=worktree,
    )
    event["ts"] = ts.isoformat().replace("+00:00", "Z")
    return event


# -- AC1-HP: a real observation becomes durable structured evidence ----------


def test_record_appends_one_valid_event_and_reports_it(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    result, code = overlap_record(journal=journal, stdin=PAYLOAD)
    assert code == 0
    assert result["recorded"] is True
    assert result["fold"]["distinct_observations"] == 1
    # The journal now holds exactly one schema-valid overlap line.
    lines = [json.loads(l) for l in journal.read_text().splitlines() if l.strip()]
    assert len(lines) == 1
    assert lines[0]["type"] == "worktree_overlap_observed"
    assert lines[0]["data"]["observation_id"] == result["observation_id"]


# -- AC4-HP: the report folds recurrence without raw-line inflation ----------


def test_repeated_delivery_of_one_observation_dedups(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    for _ in range(3):
        overlap_record(journal=journal, stdin=PAYLOAD)
    report, code = overlaps_report(journal=journal)
    assert code == 0
    assert report["distinct_observations"] == 1, "three deliveries -> one observation"
    assert report["coverage"]["journal_lines"] == 3, "raw lines preserved, just deduped"


def test_distinct_observations_count_and_per_worktree(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    append_event(
        _overlap_event(observer="o1", peers=["a"], worktree="/r/.git/wt1", ts=NOW),
        journal,
    )
    append_event(
        _overlap_event(observer="o2", peers=["b"], worktree="/r/.git/wt2", ts=NOW),
        journal,
    )
    report, _ = overlaps_report(journal=journal, now=NOW)
    assert report["distinct_observations"] == 2
    assert report["distinct_worktrees"] == 2
    assert report["distinct_observers"] == 2
    assert report["per_worktree"] == {"/r/.git/wt1": 1, "/r/.git/wt2": 1}
    assert report["recurrence_threshold_met"] is False  # 2 < 3


def test_recurrence_threshold_crosses_at_three(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    for i, wt in enumerate(["/r/.git/wt1", "/r/.git/wt2", "/r/.git/wt1"]):
        append_event(
            _overlap_event(observer=f"o{i}", peers=[f"p{i}"], worktree=wt, ts=NOW),
            journal,
        )
    report, _ = overlaps_report(journal=journal, now=NOW)
    assert report["distinct_observations"] == 3
    assert report["recurrence_threshold_met"] is True
    text = render_overlaps_text(report)
    assert "recurrence reached 3/3" in text
    assert "Stage 3" in text


def test_window_excludes_observations_older_than_since_days(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    old = NOW - timedelta(days=40)
    append_event(_overlap_event(observer="old", peers=["a"], worktree="/r/.git/wt1", ts=old), journal)
    append_event(_overlap_event(observer="new", peers=["b"], worktree="/r/.git/wt2", ts=NOW), journal)
    report, _ = overlaps_report(since_days=28, journal=journal, now=NOW)
    assert report["distinct_observations"] == 1, "the 40-day-old observation is out of window"


# -- AC7-ERR: broken report evidence cannot read as zero -------------------


def test_missing_journal_is_no_data_and_exits_zero(tmp_path: Path) -> None:
    report, code = overlaps_report(journal=tmp_path / "events.jsonl", now=NOW)
    assert report["state"] == "no_data"
    assert code == 0, "a valid empty journal is not an error"
    assert "no worktree overlap" in render_overlaps_text(report)


def test_malformed_line_makes_report_partial_and_exits_nonzero(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    append_event(_overlap_event(observer="o1", peers=["a"], worktree="/r/.git/wt1", ts=NOW), journal)
    journal.write_text(journal.read_text() + "not-valid-json\n", encoding="utf-8")
    report, code = overlaps_report(journal=journal, now=NOW)
    assert report["state"] == "partial"
    assert code == 1, "partial evidence must not read as zero"
    assert report["coverage"]["malformed_lines"] == 1
    assert report["distinct_observations"] == 1, "the valid line still counts"


def test_unreadable_journal_is_unknown_and_exits_nonzero(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    journal = tmp_path / "events.jsonl"
    journal.write_text("{}", encoding="utf-8")
    real_read_text = Path.read_text

    def _boom(self, *a, **kw):
        if self == journal:
            raise PermissionError("simulated")
        return real_read_text(self, *a, **kw)

    monkeypatch.setattr(Path, "read_text", _boom)
    report, code = overlaps_report(journal=journal, now=NOW)
    assert report["state"] == "unknown"
    assert code == 1


# -- AC6-ERR: recording failure is loud and non-blocking --------------------
#
# The carrier owns exit-zero; overlap_record must never raise. It reports
# recording status and a reason instead.


def test_record_invalid_input_is_unrecorded_not_raised(tmp_path: Path) -> None:
    result, code = overlap_record(journal=tmp_path / "e.jsonl", stdin="not-json", now=NOW)
    assert code == 0
    assert result["recorded"] is False
    assert result["record_reason"] == "invalid-input"


def test_record_empty_peers_is_unrecorded(tmp_path: Path) -> None:
    payload = '{"observer_session_id":"o","peer_session_ids":[],"worktree_git_dir":"/r/.git/wt1","repository_common_dir":"/r/.git","live_window_seconds":120}'
    result, code = overlap_record(journal=tmp_path / "e.jsonl", stdin=payload, now=NOW)
    assert code == 0
    assert result["recorded"] is False
    assert result["record_reason"] == "invalid-input"


def test_record_schema_rejection_is_unrecorded(tmp_path: Path) -> None:
    # Peers present but the repository identity is missing -> the typed builder
    # rejects it; overlap_record surfaces schema-rejected rather than crashing.
    payload = '{"observer_session_id":"o","peer_session_ids":["p"],"worktree_git_dir":"/r/.git/wt1","repository_common_dir":"","live_window_seconds":120}'
    result, code = overlap_record(journal=tmp_path / "e.jsonl", stdin=payload, now=NOW)
    assert code == 0
    assert result["recorded"] is False
    assert result["record_reason"].startswith("schema-rejected")


def test_record_lock_timeout_is_unrecorded(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    def _timeout(*a, **kw):
        raise TimeoutError("simulated lock contention")

    monkeypatch.setattr("fno.worktree_cli.overlaps.append_event", _timeout)
    result, code = overlap_record(journal=tmp_path / "e.jsonl", stdin=PAYLOAD, now=NOW)
    assert code == 0
    assert result["recorded"] is False
    assert result["record_reason"] == "lock-timeout"


def test_record_count_unavailable_when_fold_degrades(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC6-ERR second half: append succeeds but the fold read fails -> the
    carrier must distinguish count-unavailable from unrecorded. The recording
    stays true; only the fold state degrades."""
    journal = tmp_path / "events.jsonl"

    real_read = overlaps_report.__globals__["_read_overlap_events"]

    def _fail(_journal):
        from fno.worktree_cli.overlaps import OverlapReadError
        raise OverlapReadError({"path": str(_journal), "state": "unknown", "error": "simulated"})

    monkeypatch.setattr("fno.worktree_cli.overlaps._read_overlap_events", _fail)
    result, code = overlap_record(journal=journal, stdin=PAYLOAD, now=NOW)
    assert code == 0
    assert result["recorded"] is True, "the append succeeded before the fold read"
    assert result["fold"]["state"] == "unknown", "fold degraded, recording did not"
    monkeypatch.setattr("fno.worktree_cli.overlaps._read_overlap_events", real_read)


# -- AC8-FR: evidence survives worktree cleanup -----------------------------


def test_report_reads_global_journal_regardless_of_cwd(tmp_path: Path) -> None:
    """The report's default journal is the machine-global events.jsonl, not a
    worktree-local file, so archiving a feature worktree cannot erase evidence."""
    # overlaps_report() with no journal arg resolves global_events_json(); this
    # pins that the default equals the canonical global path (covers the
    # test_paths.py plan item in the report's natural home).
    from inspect import signature

    params = signature(overlaps_report).parameters
    assert "journal" in params, "report must accept an injectable journal for tests"
    assert global_events_json().name == "events.jsonl"


# -- pure fold: observation id is the dedup key -----------------------------


def test_fold_dedups_by_observation_id_not_raw_line() -> None:
    one = _overlap_event(observer="o1", peers=["a"], worktree="/r/.git/wt1", ts=NOW)
    folded = fold_overlap_events([one, one, one], since_days=28, now=NOW)
    assert folded["distinct_observations"] == 1
    assert folded["recurrence_threshold_met"] is False


def test_fold_ignores_lines_missing_observation_id() -> None:
    junk = {"type": "worktree_overlap_observed", "data": {"peer_session_ids": ["x"]}, "ts": NOW.isoformat()}
    folded = fold_overlap_events([junk], since_days=28, now=NOW)
    assert folded["distinct_observations"] == 0
