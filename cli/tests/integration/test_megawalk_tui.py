"""Tests for ``fno megawalk watch`` live TUI (repointed in task 2.4).

The TUI now reads from canonical events.jsonl (source "loop") instead of
megawalk-state.md / megawalk-events.jsonl. Walk state is derived from
WalkState.from_events rather than loaded from a YAML frontmatter file.

Snapshot-style assertions check that headline fields appear in rendered output.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from rich.console import Console

from fno.megawalk_tui import (
    _format_elapsed,
    _render_one_frame,
    _tail_jsonl,
    WalkState,
    build_layout,
)


# ---------------------------------------------------------------------------
# Time helpers
# ---------------------------------------------------------------------------


def _frozen_now(monkeypatch):
    """Freeze _now_iso inside megawalk_tui to a fixed instant."""
    fixed_iso = "2026-04-29T12:34:56+00:00"
    monkeypatch.setattr(
        "fno.megawalk_tui._now_iso", lambda: fixed_iso
    )


def test_format_elapsed_seconds():
    assert _format_elapsed(42) == "42s"


def test_format_elapsed_minutes():
    assert _format_elapsed(125) == "2m 5s"


def test_format_elapsed_hours():
    assert _format_elapsed(3725) == "1h 2m"


def test_format_elapsed_zero():
    assert _format_elapsed(0) == "0s"


def test_format_elapsed_negative_clamps_to_zero():
    assert _format_elapsed(-5) == "0s"


# ---------------------------------------------------------------------------
# _tail_jsonl
# ---------------------------------------------------------------------------


def test_tail_jsonl_returns_last_n_lines(tmp_path):
    p = tmp_path / "events.jsonl"
    lines = [
        json.dumps({"ts": f"2026-04-29T12:00:0{i}", "type": f"evt{i}", "source": "loop"})
        for i in range(5)
    ]
    p.write_text("\n".join(lines) + "\n")
    result = _tail_jsonl(p, n=3)
    assert len(result) == 3
    assert result[0]["type"] == "evt2"
    assert result[2]["type"] == "evt4"


def test_tail_jsonl_tolerates_corrupted_lines(tmp_path):
    """Truncated mid-write lines must be dropped silently."""
    p = tmp_path / "events.jsonl"
    p.write_text(
        json.dumps({"ts": "2026-04-29T12:34:56", "type": "ok", "source": "loop"})
        + "\n"
        + "not valid json\n"
        + json.dumps({"ts": "2026-04-29T12:35:01", "type": "also_ok", "source": "loop"})
        + "\n"
    )
    result = _tail_jsonl(p, n=10)
    assert len(result) == 2
    assert result[0]["type"] == "ok"
    assert result[1]["type"] == "also_ok"


def test_tail_jsonl_missing_file_returns_empty(tmp_path):
    p = tmp_path / "does-not-exist.jsonl"
    assert _tail_jsonl(p, n=10) == []


# ---------------------------------------------------------------------------
# WalkState.from_events (journal replay)
# ---------------------------------------------------------------------------

def _make_loop_event(kind: str, data: dict, ts: str = "2026-04-29T12:34:56+00:00") -> dict:
    return {"ts": ts, "type": kind, "source": "loop", "data": data}


def test_AC1_HP_walk_state_running_after_dispatch():
    """AC1-HP: a dispatched unit appears in in_flight with 'running' status."""
    events = [
        _make_loop_event("loop_unit_dispatched", {"unit_id": "ab-abc12345"}, ts="2026-04-29T10:00:00+00:00"),
    ]
    walk = WalkState.from_events(events)
    assert walk.status == "running"
    assert "ab-abc12345" in walk.in_flight
    assert walk.started_at == "2026-04-29T10:00:00+00:00"


def test_AC2_HP_walk_state_unit_closed_after_node_closed():
    """AC2-HP: a node_closed event removes the unit from in_flight."""
    events = [
        _make_loop_event("loop_unit_dispatched", {"unit_id": "ab-abc12345"}),
        _make_loop_event("node_closed", {"unit_id": "ab-abc12345", "close": "closed"}),
    ]
    walk = WalkState.from_events(events)
    assert "ab-abc12345" not in walk.in_flight
    assert ("ab-abc12345", "closed") in walk.recently_closed


def test_AC3_HP_walk_state_paused_after_walk_paused():
    """AC3-HP: a walk_paused event sets status=paused with policy details."""
    events = [
        _make_loop_event("loop_unit_dispatched", {"unit_id": "ab-abc12345"}),
        _make_loop_event("node_closed", {"unit_id": "ab-abc12345", "close": "parked"}),
        _make_loop_event("walk_paused", {"policy": "consecutive_failures", "detail": "ab-abc12345"}),
    ]
    walk = WalkState.from_events(events)
    assert walk.status == "paused"
    assert walk.pause_policy == "consecutive_failures"
    assert "ab-abc12345" in (walk.pause_detail or "")


def test_AC4_HP_walk_state_terminated_after_loop_terminated():
    """AC4-HP: a loop_terminated event sets status=terminated and clears in_flight."""
    events = [
        _make_loop_event("loop_unit_dispatched", {"unit_id": "ab-abc12345"}),
        _make_loop_event("loop_terminated", {"reason": "NoWork"}),
    ]
    walk = WalkState.from_events(events)
    assert walk.status == "terminated"
    assert walk.terminated_reason == "NoWork"
    assert walk.in_flight == {}


def test_AC5_HP_empty_events_starting_state():
    """AC5-HP: no events means 'starting' state with no in-flight units."""
    walk = WalkState.from_events([])
    assert walk.status == "starting"
    assert walk.in_flight == {}


def test_AC6_HP_parked_outcome_recorded():
    """AC6-HP: parked close outcome is recorded in recently_closed."""
    events = [
        _make_loop_event("loop_unit_dispatched", {"unit_id": "ab-parked1"}),
        _make_loop_event("node_closed", {"unit_id": "ab-parked1", "close": "parked"}),
    ]
    walk = WalkState.from_events(events)
    assert ("ab-parked1", "parked") in walk.recently_closed


# ---------------------------------------------------------------------------
# build_layout
# ---------------------------------------------------------------------------


def test_build_layout_renders_in_flight_node(tmp_path, monkeypatch):
    """build_layout shows in-flight unit id and dispatch time."""
    _frozen_now(monkeypatch)
    events = [
        _make_loop_event(
            "loop_unit_dispatched",
            {"unit_id": "ab-abc12345"},
            ts="2026-04-29T12:30:00+00:00",
        ),
    ]
    walk = WalkState.from_events(events)
    events_path = tmp_path / "events.jsonl"
    events_path.write_text("")
    console = Console(record=True, width=140)
    console.print(build_layout(walk, events_path))
    output = console.export_text()
    assert "ab-abc12345" in output
    assert "running" in output


def test_build_layout_renders_recently_closed(tmp_path, monkeypatch):
    """build_layout shows recently closed unit with its outcome."""
    _frozen_now(monkeypatch)
    events = [
        _make_loop_event("loop_unit_dispatched", {"unit_id": "ab-def67890"}),
        _make_loop_event("node_closed", {"unit_id": "ab-def67890", "close": "closed"}),
    ]
    walk = WalkState.from_events(events)
    events_path = tmp_path / "events.jsonl"
    events_path.write_text("")
    console = Console(record=True, width=140)
    console.print(build_layout(walk, events_path))
    output = console.export_text()
    assert "ab-def67890" in output
    assert "closed" in output


def test_build_layout_renders_events_tail(tmp_path, monkeypatch):
    """build_layout shows raw events from events.jsonl."""
    _frozen_now(monkeypatch)
    walk = WalkState.from_events([])
    events_path = tmp_path / "events.jsonl"
    events_path.write_text(
        json.dumps({"ts": "2026-04-29T12:34:56+00:00", "type": "loop_unit_dispatched", "source": "loop", "data": {"unit_id": "ab-xyz"}})
        + "\n"
    )
    console = Console(record=True, width=140)
    console.print(build_layout(walk, events_path))
    output = console.export_text()
    assert "loop_unit_dispatched" in output


def test_build_layout_renders_paused_status(tmp_path, monkeypatch):
    """build_layout shows paused policy in the header."""
    _frozen_now(monkeypatch)
    events = [
        _make_loop_event("walk_paused", {"policy": "p0_failed", "detail": "ab-critical"}),
    ]
    walk = WalkState.from_events(events)
    events_path = tmp_path / "events.jsonl"
    events_path.write_text("")
    console = Console(record=True, width=140)
    console.print(build_layout(walk, events_path))
    output = console.export_text()
    assert "paused" in output
    assert "p0_failed" in output


def test_build_layout_handles_empty_walk(tmp_path, monkeypatch):
    """build_layout renders without error when nothing has happened yet."""
    _frozen_now(monkeypatch)
    walk = WalkState.from_events([])
    events_path = tmp_path / "events.jsonl"
    events_path.write_text("")
    console = Console(record=True, width=140)
    console.print(build_layout(walk, events_path))
    output = console.export_text()
    assert "Walk" in output or "starting" in output
    assert "In-flight (0)" in output


# ---------------------------------------------------------------------------
# _render_one_frame (repointed: takes events_path only)
# ---------------------------------------------------------------------------


def test_render_one_frame_returns_waiting_when_journal_missing(tmp_path):
    """AC-VERIFY: missing events.jsonl renders an explicit waiting message, not blank."""
    events_path = tmp_path / "events.jsonl"
    panel_or_layout = _render_one_frame(events_path)
    console = Console(record=True, width=120)
    console.print(panel_or_layout)
    output = console.export_text()
    assert "Waiting" in output or "fno-agents loop run" in output


def test_render_one_frame_renders_running_walk(tmp_path, monkeypatch):
    """With a dispatched unit in events.jsonl, frame shows running state."""
    _frozen_now(monkeypatch)
    events_path = tmp_path / "events.jsonl"
    events_path.write_text(
        json.dumps({
            "ts": "2026-04-29T10:00:00+00:00",
            "type": "loop_unit_dispatched",
            "source": "loop",
            "data": {"unit_id": "ab-test001"},
        }) + "\n"
    )
    panel_or_layout = _render_one_frame(events_path)
    console = Console(record=True, width=140)
    console.print(panel_or_layout)
    output = console.export_text()
    assert "ab-test001" in output or "running" in output


def test_render_one_frame_shows_terminated(tmp_path, monkeypatch):
    """loop_terminated event makes frame show terminated status."""
    _frozen_now(monkeypatch)
    events_path = tmp_path / "events.jsonl"
    events_path.write_text(
        json.dumps({
            "ts": "2026-04-29T12:00:00+00:00",
            "type": "loop_terminated",
            "source": "loop",
            "data": {"reason": "NoWork"},
        }) + "\n"
    )
    panel_or_layout = _render_one_frame(events_path)
    console = Console(record=True, width=140)
    console.print(panel_or_layout)
    output = console.export_text()
    assert "terminated" in output or "NoWork" in output


# ---------------------------------------------------------------------------
# CLI registration
# ---------------------------------------------------------------------------


def test_megawalk_watch_subcommand_registered(monkeypatch):
    """`fno megawalk watch` should appear in the megawalk subapp."""
    from typer.testing import CliRunner
    from fno.cli import app

    monkeypatch.setenv("COLUMNS", "240")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("TERM", "dumb")
    runner = CliRunner()
    result = runner.invoke(app, ["megawalk", "--help"])
    assert result.exit_code == 0
    assert "watch" in result.stdout


def test_megawalk_bare_exits_12_and_mentions_front_door(monkeypatch):
    """AC1-HP: bare `fno megawalk` callback exits 12 and mentions Rust loop."""
    from typer.testing import CliRunner
    from fno.megawalk import app as megawalk_app

    runner = CliRunner()
    result = runner.invoke(megawalk_app, [], catch_exceptions=False)
    assert result.exit_code == 12
    combined = (result.output or "")
    assert "fno-agents loop run --driver megawalk" in combined
