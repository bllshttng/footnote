"""Acceptance tests for the rotation-aware event journal query.

find is native now: Python resolves the journals, the binary folds and
answers. The contract carries a coverage receipt, and a zero that coverage
cannot back exits 3.
"""
from __future__ import annotations

import json
from pathlib import Path

from typer.testing import CliRunner

from fno.events.cli import cli as event_cli


def _row(path: Path, row: dict) -> None:
    path.write_text(json.dumps(row) + "\n", encoding="utf-8")


def _patch_live_journals(monkeypatch, live: Path) -> None:
    import fno.paths as paths

    monkeypatch.setattr(paths, "global_events_json", lambda: live)
    monkeypatch.setattr(paths, "project_events_json", lambda: live)
    monkeypatch.setattr(paths, "agents_home_dir", lambda: live.parent)


def test_find_counts_rotated_rows(tmp_path: Path, monkeypatch) -> None:
    """A match in a retained segment is not a false zero."""
    live = tmp_path / "events.jsonl"
    rotated = live.with_name("events.jsonl.1")
    _row(live.with_name("events.jsonl.2"),
         {"ts": "2026-08-01T00:00:00Z", "type": "other", "data": {}})
    _row(rotated, {"ts": "2026-08-02T00:00:00Z", "kind": "operator_decision", "data": {}})
    _row(live, {"ts": "2026-08-03T00:00:00Z", "type": "other", "data": {}})
    monkeypatch.setattr("fno.paths.event_journals", lambda: [live])

    result = CliRunner().invoke(event_cli, ["find", "operator_decision", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["match_count"] == 1
    assert payload["file_count"] == 1
    assert payload["row_count"] == 3
    assert payload["fields_searched"] == ["type", "kind", "event"]


def test_find_kinds_reports_both_envelope_fields(tmp_path: Path, monkeypatch) -> None:
    """Mixed envelopes report under the field that named them."""
    live = tmp_path / "events.jsonl"
    _row(live, {"ts": "2026-08-03T00:00:00Z", "type": "typed_event", "data": {}})
    with live.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps({"ts": "2026-08-03T01:00:00Z", "kind": "legacy_event"}) + "\n")
    _patch_live_journals(monkeypatch, live)

    result = CliRunner().invoke(event_cli, ["find", "--kinds", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert "typed_event" in payload["kind_counts"]
    assert "legacy_event" in payload["kind_counts"]
    assert payload["kind_counts"]["typed_event"]["keys"]["type"] == 1
    assert payload["kind_counts"]["legacy_event"]["keys"]["kind"] == 1


def test_find_zero_with_complete_coverage_exits_zero(tmp_path: Path, monkeypatch) -> None:
    """A zero inside a fully proven window is a measured zero, not a refusal."""
    import sqlite3
    import subprocess
    import time

    from fno.events.store_client import resolve_native_bin, store_db_path

    live = tmp_path / "events.jsonl"
    _row(live, {"ts": "2026-08-03T00:00:00Z", "type": "other", "data": {}})
    _patch_live_journals(monkeypatch, live)
    # The store must predate the asked window: a first-open stamp answers
    # "partial" for every since (nothing was observed before the open).
    subprocess.run(
        [resolve_native_bin(), "doctor", "event", "import", "--events", str(live)],
        check=True, capture_output=True,
    )
    epoch = int((time.time() - 2 * 3600) * 1000)
    conn = sqlite3.connect(store_db_path(live))
    conn.execute(
        "UPDATE events_meta SET value = ? WHERE key = 'coverage_complete_since_ms'",
        (str(epoch),),
    )
    conn.commit()
    conn.close()

    result = CliRunner().invoke(event_cli, ["find", "operator_decision", "--since", "1h"])

    assert result.exit_code == 0, result.output
    assert "no matches" in result.output
    assert "complete since" in result.output


def test_find_zero_beyond_coverage_is_refused(tmp_path: Path, monkeypatch) -> None:
    """A zero reaching before the epoch is refused, never a confident zero."""
    live = tmp_path / "events.jsonl"
    _row(live, {"ts": "2026-08-03T00:00:00Z", "type": "other", "data": {}})
    _patch_live_journals(monkeypatch, live)

    result = CliRunner().invoke(event_cli, ["find", "operator_decision", "--since", "30d"])

    assert result.exit_code == 3, result.output
    assert "count refused" in result.output


def test_find_zero_beyond_coverage_with_matches_reads_partial(tmp_path: Path, monkeypatch) -> None:
    """Matches are proof on their own; partial coverage still exits 0."""
    from datetime import datetime, timedelta, timezone

    live = tmp_path / "events.jsonl"
    recent = datetime.now(timezone.utc) - timedelta(days=1)
    _row(live, {"ts": recent.isoformat().replace("+00:00", "Z"),
                "type": "operator_decision", "data": {}})
    _patch_live_journals(monkeypatch, live)

    result = CliRunner().invoke(
        event_cli, ["find", "operator_decision", "--since", "30d", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["match_count"] == 1
    assert payload["coverage"]["status"] == "partial"
    assert payload["complete_count"] is None
    assert payload["coverage"]["complete_since"]


def test_find_absent_store_refuses_the_zero(tmp_path: Path, monkeypatch) -> None:
    """A journal with no store proves nothing; its zero is refused."""
    missing = tmp_path / "events.jsonl"
    monkeypatch.setattr("fno.paths.event_journals", lambda: [missing])

    result = CliRunner().invoke(event_cli, ["find", "operator_decision", "--json"])

    assert result.exit_code == 3, result.output
    payload = json.loads(result.output)
    assert payload["coverage"]["status"] == "unreadable"
    assert payload["file_count"] == 1


def test_agent_raw_inject_builder_records_verb_and_self_send() -> None:
    """AC13/AC14: Python transport rows carry both raw-inject facts."""
    from fno.events import agent_raw_inject

    slash = agent_raw_inject(
        target_session="session-1",
        payload="/compact /tmp/handoff.md",
        harness="claude",
        lane="mux-pane",
        self_send=True,
    )
    prose = agent_raw_inject(
        target_session="session-1",
        payload="plain text",
        harness="claude",
        lane="mux-pane",
    )

    assert slash["data"]["verb"] == "/compact"
    assert slash["data"]["self_send"] is True
    assert prose["data"]["verb"] is None
    assert prose["data"]["self_send"] is False
