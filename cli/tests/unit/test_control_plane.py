"""The Python writer for `control_plane_tick` rows (x-1b88)."""
from __future__ import annotations

from pathlib import Path

from fno.control_plane import emit_tick, scheduler_from_env


def test_emit_tick_writes_a_schema_valid_row(tmp_path, monkeypatch):
    journal = tmp_path / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(journal))

    ok = emit_tick(
        "watchdog",
        scheduler="launchd:sh.fno.pr-watcher",
        interval_s=600,
        acted=2,
        detail="wake=1 report=1",
    )

    assert ok is True
    from tests._event_rows import event_rows

    rows = event_rows(journal)
    assert len(rows) == 1
    row = rows[0]
    assert row["type"] == "control_plane_tick"
    assert row["data"] == {
        "arm": "watchdog",
        "scheduler": "launchd:sh.fno.pr-watcher",
        "acted": 2,
        "interval_s": 600,
        "detail": "wake=1 report=1",
    }


def test_emit_tick_omits_null_skip_reason(tmp_path, monkeypatch):
    journal = tmp_path / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(journal))

    assert emit_tick(
        "king_wake", scheduler="daemon", interval_s=900, skip_reason="no_crowned_target"
    )

    from tests._event_rows import event_rows

    row = event_rows(journal)[0]
    assert row["data"]["skip_reason"] == "no_crowned_target"
    assert "detail" not in row["data"]


def test_emit_tick_stores_a_long_failure_detail_whole(tmp_path, monkeypatch):
    """A failure row's refusal survives whole. One 4000-char bound per row
    keeps a pathological crash stderr from writing a giant journal row; no
    200-char window that cut a refusal mid-flag and lost the cause."""
    journal = tmp_path / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(journal))

    refusal = "fno agents spawn: refusing; strict routing refuses the harness default" + "x" * 2000
    assert emit_tick("auto_continue", scheduler="session", interval_s=1800,
                     skip_reason="spawn-failed", detail=refusal)

    from tests._event_rows import event_rows

    row = event_rows(journal)[0]
    assert row["data"]["detail"] == refusal

    assert emit_tick("auto_continue", scheduler="session", interval_s=1800,
                     skip_reason="spawn-failed", detail="y" * 5000)
    capped = event_rows(journal)[1]
    assert len(capped["data"]["detail"]) == 4000


def test_emit_tick_never_raises_on_a_dead_journal(tmp_path):
    # A path whose parent is a FILE: every write fails; the arm it observes
    # must not break.
    blocker = tmp_path / "blocker"
    blocker.write_text("not a dir")
    ok = emit_tick(
        "stop_hook",
        scheduler="hook:target-stop-hook",
        interval_s=0,
        events_path=blocker / "events.jsonl",
    )
    assert ok is False


def test_scheduler_from_env_defaults_to_session(monkeypatch):
    monkeypatch.delenv("FNO_CONTROL_PLANE_SCHEDULER", raising=False)
    assert scheduler_from_env() == "session"
    monkeypatch.setenv("FNO_CONTROL_PLANE_SCHEDULER", "launchd:sh.fno.autocontinue")
    assert scheduler_from_env() == "launchd:sh.fno.autocontinue"
