"""Macro-eval folding over the existing event journals."""
from __future__ import annotations

import json
from datetime import datetime, timezone

from fno.evals.macro import build_leaderboard, load_events


def _event(ts: str, event_type: str, *, session: str | None = None,
           node: str | None = None, **data: object) -> dict:
    payload = dict(data)
    if session is not None:
        payload["session_id"] = session
    if node is not None:
        payload["node_id"] = node
    return {"ts": ts, "type": event_type, "data": payload}


def test_leaderboard_ranks_upstream_suspect_for_repeated_failure() -> None:
    rows = [
        _event("2026-09-12T10:00:00Z", "loop_check_watch_idle", session="s1", node="n1", reason="ci"),
        _event("2026-09-12T10:01:00Z", "termination", session="s1", node="n1", reason="Budget"),
        _event("2026-09-12T11:00:00Z", "loop_check_watch_idle", session="s2", node="n2", reason="ci"),
        _event("2026-09-12T11:01:00Z", "termination", session="s2", node="n2", reason="Budget"),
        _event("2026-09-12T12:00:00Z", "loop_check_watch_idle", session="s3", node="n3", reason="ci"),
    ]

    result = build_leaderboard(rows)
    budget = next(item for item in result["leaderboard"] if item["pattern"] == "termination:Budget")

    assert budget["sessions"] == 2
    assert budget["count"] == 2
    assert budget["nodes"] == 2
    assert budget["suspects"][0]["pattern"] == "loop_check_watch_idle:ci"
    assert budget["suspects"][0]["lift"] > 1


def test_leaderboard_keeps_unassigned_rows_and_filters_noise() -> None:
    rows = [
        _event("2026-09-12T10:00:00Z", "termination", reason="Budget"),
        _event("2026-09-12T10:01:00Z", "guard_decision", session="s1", reason="allow"),
        _event("2026-09-12T10:02:00Z", "termination", session="s1", reason="DonePRGreen"),
    ]

    result = build_leaderboard(rows)
    patterns = {item["pattern"] for item in result["leaderboard"]}
    budget = next(item for item in result["leaderboard"] if item["pattern"] == "termination:Budget")
    assert patterns == {"termination:Budget"}
    assert budget["count"] == 1
    assert budget["unassigned"] == 1

    all_result = build_leaderboard(rows, include_all=True)
    all_patterns = {item["pattern"] for item in all_result["leaderboard"]}
    assert "guard_decision:allow" in all_patterns
    assert "termination:DonePRGreen" in all_patterns


def test_load_events_reports_malformed_lines_and_returns_valid_rows(tmp_path) -> None:
    journal = tmp_path / "events.jsonl"
    journal.write_text(
        json.dumps(_event("2026-09-12T10:00:00Z", "termination", reason="Budget"))
        + "\nnot json\n"
        + json.dumps(_event("2026-09-12T10:01:00Z", "termination", reason="NoProgress"))
        + "\n",
        encoding="utf-8",
    )

    rows, coverage = load_events([journal], since=datetime(2026, 9, 1, tzinfo=timezone.utc))

    assert len(rows) == 2
    assert coverage["malformed_lines"] == 1
    assert coverage["complete"] is False
