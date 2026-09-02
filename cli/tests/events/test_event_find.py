"""Acceptance tests for the rotation-aware event journal query."""
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


def test_find_counts_rotated_rows_and_reports_denominator(tmp_path: Path, monkeypatch) -> None:
    """AC1/AC2/AC7: a match in a retained segment is not a false zero."""
    live = tmp_path / "events.jsonl"
    rotated = live.with_name("events.jsonl.1")
    oldest = live.with_name("events.jsonl.2")
    _row(oldest, {"ts": "2026-08-01T00:00:00Z", "type": "other", "data": {}})
    _row(rotated, {"ts": "2026-08-02T00:00:00Z", "kind": "operator_decision", "session_id": "s-mid"})
    _row(live, {"ts": "2026-08-03T00:00:00Z", "type": "other", "data": {}})
    _patch_live_journals(monkeypatch, live)

    result = CliRunner().invoke(event_cli, ["find", "operator_decision", "--session", "s-mid"])

    assert result.exit_code == 0, result.output
    assert "1 matches in 3 rows across 3 files" in result.output
    assert str(rotated) in result.output
    assert "window 2026-08-01T00:00:00Z .. 2026-08-03T00:00:00Z" in result.output
    assert "fields searched: type, kind, event" in result.output
    assert "key: kind" in result.output


def test_event_journals_scans_rotations_beside_resolved_symlink(tmp_path: Path, monkeypatch) -> None:
    """The rotation siblings belong beside the resolved target, not the link."""
    canonical = tmp_path / "canonical" / "events.jsonl"
    canonical.parent.mkdir()
    canonical.write_text("", encoding="utf-8")
    canonical.with_name("events.jsonl.1").write_text("", encoding="utf-8")
    link = tmp_path / "worktree" / "events.jsonl"
    link.parent.mkdir()
    link.symlink_to(canonical)
    _patch_live_journals(monkeypatch, link)

    import fno.paths as paths

    assert paths.event_journals() == [canonical.with_name("events.jsonl.1"), canonical]


def test_find_kinds_reports_the_key_used_without_blank_kinds(tmp_path: Path, monkeypatch) -> None:
    """AC3: mixed envelopes retain the actual key used for each row."""
    live = tmp_path / "events.jsonl"
    _row(live, {"ts": "2026-08-03T00:00:00Z", "type": "typed_event", "data": {}})
    with live.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps({"ts": "2026-08-03T01:00:00Z", "kind": "legacy_event"}) + "\n")
    _patch_live_journals(monkeypatch, live)

    result = CliRunner().invoke(event_cli, ["find", "--kinds"])

    assert result.exit_code == 0, result.output
    assert "typed_event" in result.output
    assert "legacy_event" in result.output
    assert "key: type" in result.output
    assert "key: kind" in result.output
    assert "blank" not in result.output.lower()


def test_find_unreadable_segment_is_not_counted_as_zero(tmp_path: Path, monkeypatch) -> None:
    """AC4/AC5: a segment that disappears is an explicit partial read."""
    missing = tmp_path / "events.jsonl.1"
    monkeypatch.setattr("fno.paths.event_journals", lambda: [missing])

    result = CliRunner().invoke(event_cli, ["find", "operator_decision"])

    assert result.exit_code == 3
    assert "rotated-away" in result.output
    assert str(missing) in result.output


def test_find_zero_still_reports_denominator_and_window(tmp_path: Path, monkeypatch) -> None:
    """AC6: zero is a measured result, never a bare absence."""
    live = tmp_path / "events.jsonl"
    _row(live, {"ts": "2026-08-03T00:00:00Z", "type": "other", "data": {}})
    _patch_live_journals(monkeypatch, live)

    result = CliRunner().invoke(event_cli, ["find", "operator_decision"])

    assert result.exit_code == 0, result.output
    assert "0 matches in 1 rows across 1 files" in result.output
    assert "window 2026-08-03T00:00:00Z .. 2026-08-03T00:00:00Z" in result.output
    assert "fields searched: type, kind, event" in result.output


def test_find_json_contains_matches_and_file_denominators(tmp_path: Path, monkeypatch) -> None:
    """The machine-readable form keeps the same positive coverage evidence."""
    live = tmp_path / "events.jsonl"
    _row(live, {"ts": "2026-08-03T00:00:00Z", "type": "operator_decision", "data": {}})
    _patch_live_journals(monkeypatch, live)

    result = CliRunner().invoke(event_cli, ["find", "operator_decision", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["match_count"] == 1
    assert payload["row_count"] == 1
    assert payload["fields_searched"] == ["type", "kind", "event"]
    assert payload["files"][0]["matches"] == 1
    assert payload["files"][0]["keys"]["type"] == 1


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
