"""Retro gate_escape aggregation: ranked-by-reason output + emit-failure
visibility (x-f894, AC6-FR / AC7-FR)."""
from __future__ import annotations

import json
from pathlib import Path

from fno.retro.gate_escape import render_gate_escapes, summarize_gate_escapes


def _write_events(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "\n".join(
            json.dumps({"ts": "2026-07-07T00:00:00Z", "type": "gate_escape",
                        "source": "backlog", "data": r})
            for r in rows
        )
        + "\n"
    )


def test_ac6_fr_ranked_by_reason_most_frequent_first(tmp_path):
    ev = tmp_path / "events.jsonl"
    _write_events(ev, [
        {"reason": "dead-bot", "pr": 201},
        {"reason": "dead-bot", "pr": 218},
        {"reason": "dead-bot", "pr": 224},
        {"reason": "spawn-cap", "pr": 212},
        {"reason": "spawn-cap", "pr": 215},
        {"reason": "stale-base", "pr": 230},
    ])
    s = summarize_gate_escapes(ev)
    assert s.total == 6
    # Most-frequent first.
    assert [r for r, _ in s.by_reason] == ["dead-bot", "spawn-cap", "stale-base"]
    assert s.by_reason[0] == ("dead-bot", 3)
    assert s.prs_by_reason["dead-bot"] == [201, 218, 224]
    line = render_gate_escapes(s)[0]
    assert "dead-bot=3" in line
    assert line.index("dead-bot=3") < line.index("spawn-cap=2")
    assert "PR #201" in line


def test_empty_log_is_zero_by_reason_not_error(tmp_path):
    s = summarize_gate_escapes(tmp_path / "events.jsonl")  # file absent
    assert s.total == 0
    assert render_gate_escapes(s) == ["gate_escapes: 0 by reason"]


def test_ac7_fr_emit_failures_surfaced(tmp_path):
    ev = tmp_path / "events.jsonl"
    _write_events(ev, [{"reason": "dead-bot", "pr": 201}])
    fail = tmp_path / "gate_escape_emit_failures.jsonl"
    fail.write_text('{"reason":"dead-bot"}\n{"reason":"dead-bot"}\n')
    s = summarize_gate_escapes(ev)
    assert s.emit_failures == 2
    lines = render_gate_escapes(s)
    warn = [l for l in lines if "may under-report" in l]
    assert warn and "2 emit failure" in warn[0]


def test_no_failures_no_warn(tmp_path):
    ev = tmp_path / "events.jsonl"
    _write_events(ev, [{"reason": "spawn-cap", "pr": 5}])
    s = summarize_gate_escapes(ev)
    assert s.emit_failures == 0
    assert not any("under-report" in l for l in render_gate_escapes(s))
