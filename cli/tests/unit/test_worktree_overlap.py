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

from fno.events import ValidationError, append_event, validate, worktree_overlap_observed
from fno.worktree_cli.overlaps import (
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


# -- AC4-HP: the report folds recurrence without raw-line inflation ----------


def test_repeated_delivery_of_one_observation_dedups(tmp_path: Path) -> None:
    journal = tmp_path / "events.jsonl"
    for _ in range(3):
        overlap_record(journal=journal, stdin=PAYLOAD)
    report, code = overlaps_report(journal=journal)
    assert code == 0
    assert report["distinct_observations"] == 1, "three deliveries -> one observation"
    assert report["coverage"]["journal_lines"] == 3, "raw lines preserved, just deduped"


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


def test_malformed_line_makes_report_partial_and_exits_nonzero(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("fno.events.store_client.native_rows", lambda *a, **k: None)
    journal = tmp_path / "events.jsonl"
    journal.write_text(
        json.dumps(_overlap_event(observer="o1", peers=["a"], worktree="/r/.git/wt1", ts=NOW))
        + "\nnot-valid-json\n",
        encoding="utf-8",
    )
    report, code = overlaps_report(journal=journal, now=NOW)
    assert report["state"] == "partial"
    assert code == 1, "partial evidence must not read as zero"
    assert report["coverage"]["malformed_lines"] == 1
    assert report["distinct_observations"] == 1, "the valid line still counts"


def test_unreadable_journal_is_unknown_and_exits_nonzero(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fno.events.store_client.native_rows", lambda *a, **k: None)
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


# -- pure fold: observation id is the dedup key -----------------------------


def test_report_rejects_degenerate_window_exits_nonzero(tmp_path: Path) -> None:
    report, code = overlaps_report(since_days=0, journal=tmp_path / "e.jsonl", now=NOW)
    assert code == 1
    assert report["state"] == "unknown"


def test_validate_rejects_observation_id_not_matching_fields() -> None:
    """A hand-crafted id that does not match the field digest must fail loud."""
    event = worktree_overlap_observed(
        observer_session_id="obs-1",
        peer_session_ids=["peer-a"],
        repository_key="/r/.git",
        worktree_key="/r/.git/wt1",
    )
    # Tamper: a valid 64-hex id that is NOT the digest of these fields.
    event["data"]["observation_id"] = "0" * 64
    with pytest.raises(ValidationError):
        validate(event)


# -- carrier contract: the real Typer command accepts the carrier's invocation --
#
# The unit tests above call the Python functions directly; the hook test uses a
# fno shim. Neither exercises real flag parsing, so a missing --stdin option on
# the command would ship green while the carrier's invocation failed every time.
# This pins the carrier's exact argv against the real command.


def test_overlap_record_cli_accepts_carrier_invocation(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from typer.testing import CliRunner

    monkeypatch.setenv("FNO_HOME", str(tmp_path / "fno"))
    runner = CliRunner()
    from fno.worktree_cli.cli import app

    result = runner.invoke(
        app,
        ["overlap-record", "--stdin", "--since", "28"],
        input=PAYLOAD,
    )
    assert result.exit_code == 0, result.output
    parsed = json.loads(result.stdout.strip())
    assert parsed["recorded"] is True
    assert parsed["fold"]["distinct_observations"] == 1
    # The store beside the journal now holds one durable row; the raw file
    # may stay absent (the commit is the write boundary). The shared sandbox
    # journal also carries rows from other tests, so filter by type.
    from tests._event_rows import event_rows
    from fno.paths import global_events_json

    journal = global_events_json()
    rows = [
        r for r in event_rows(journal)
        if r.get("type") == "worktree_overlap_observed"
    ]
    assert rows, f"no worktree_overlap_observed row in {journal}"


def test_overlaps_cli_reports_and_exits_clean(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from typer.testing import CliRunner

    monkeypatch.setenv("FNO_HOME", str(tmp_path / "fno"))
    runner = CliRunner()
    from fno.worktree_cli.cli import app

    # Record one, then report.
    rec = runner.invoke(app, ["overlap-record", "--stdin"], input=PAYLOAD)
    assert rec.exit_code == 0
    rep = runner.invoke(app, ["overlaps", "--since", "28", "--json"])
    assert rep.exit_code == 0, rep.output
    report = json.loads(rep.stdout.strip())
    assert report["distinct_observations"] == 1
    # Text mode renders without raising.
    txt = runner.invoke(app, ["overlaps", "--since", "28"])
    assert txt.exit_code == 0
    assert "1 distinct" in txt.output
