"""Tests for drive-authority detection (Phase 6 Wave 4, ab-8d258ddb).

`is_drive_authority_active()` / `active_drive_sessions()` are the gate-hardening
primitive: they report whether an operator holds an interactive/step/paranoid
drive window on any agent (LD3, LD24/29). Watch windows are read-only and must
NOT count.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.drive_authority import (
    active_drive_sessions,
    is_drive_authority_active,
)
from fno.paths_testing import use_tmpdir


def _write_state(agents_dir: Path, short_id: str, pty: dict | None) -> None:
    d = agents_dir / short_id
    d.mkdir(parents=True, exist_ok=True)
    state = {"schema_version": 1, "short_id": short_id, "status": "live"}
    if pty is not None:
        state["pty"] = pty
    (d / "state.json").write_text(json.dumps(state))


def _drive(active: bool, mode: str | None, session_id: str | None = "d-1") -> dict:
    return {
        "active": True,
        "drive_active": active,
        "drive_session_id": session_id,
        "drive_mode": mode,
    }


def test_no_agents_dir_is_not_active(tmp_path: Path) -> None:
    missing = tmp_path / "nope"
    assert active_drive_sessions(missing) == []
    assert is_drive_authority_active(missing) is False


def test_interactive_step_paranoid_open_authority(tmp_path: Path) -> None:
    _write_state(tmp_path, "wkI", _drive(True, "interactive", "d-i"))
    _write_state(tmp_path, "wkS", _drive(True, "step", "d-s"))
    _write_state(tmp_path, "wkP", _drive(True, "paranoid", "d-p"))
    sessions = active_drive_sessions(tmp_path)
    assert {s["short_id"] for s in sessions} == {"wkI", "wkS", "wkP"}
    assert is_drive_authority_active(tmp_path) is True
    # Records carry mode + session_id.
    by_id = {s["short_id"]: s for s in sessions}
    assert by_id["wkS"]["mode"] == "step"
    assert by_id["wkP"]["session_id"] == "d-p"


def test_watch_does_not_open_authority(tmp_path: Path) -> None:
    # Watch never writes the state.json window in production, but even if a
    # window names mode=watch it must NOT count as authority (LD24).
    _write_state(tmp_path, "wkW", _drive(True, "watch"))
    assert active_drive_sessions(tmp_path) == []
    assert is_drive_authority_active(tmp_path) is False


def test_inactive_window_and_no_pty_are_not_active(tmp_path: Path) -> None:
    _write_state(tmp_path, "wkOff", _drive(False, "interactive"))  # drive_active False
    _write_state(tmp_path, "wkNoPty", None)  # claude-style entry, no pty
    assert active_drive_sessions(tmp_path) == []
    assert is_drive_authority_active(tmp_path) is False


def test_corrupt_or_missing_state_is_skipped(tmp_path: Path) -> None:
    # A live interactive window plus a corrupt sibling: the good one still
    # reports, the corrupt one is skipped (no crash).
    _write_state(tmp_path, "wkGood", _drive(True, "interactive"))
    bad = tmp_path / "wkBad"
    bad.mkdir(parents=True)
    (bad / "state.json").write_text("{not json")
    (tmp_path / ".orphaned").mkdir()  # dotfile dir is ignored
    assert is_drive_authority_active(tmp_path) is True
    assert [s["short_id"] for s in active_drive_sessions(tmp_path)] == ["wkGood"]


def test_unknown_mode_does_not_open_authority(tmp_path: Path) -> None:
    _write_state(tmp_path, "wkX", _drive(True, "bogus-mode"))
    assert is_drive_authority_active(tmp_path) is False


# --- operator-initiated audit tagging (cv-9def52a7) ------------------------


def test_emit_operator_initiated_envelope_and_data(tmp_path: Path) -> None:
    from fno.agents.drive_authority import emit_operator_initiated

    events = tmp_path / ".fno" / "events.jsonl"
    emit_operator_initiated(
        "backlog_done_operator_initiated",
        source="backlog",
        events_path=events,
        task_id="ab-12345678",
    )
    rec = json.loads(events.read_text().strip())
    # Envelope matches the bash emit_event siblings ({timestamp,source,type,data}),
    # NOT the validated {ts,...} stream, so an auditor greps one project stream.
    assert set(rec) == {"timestamp", "source", "type", "data"}
    assert rec["source"] == "backlog"
    assert rec["type"] == "backlog_done_operator_initiated"
    assert rec["data"] == {"task_id": "ab-12345678"}


def test_emit_operator_initiated_appends(tmp_path: Path) -> None:
    from fno.agents.drive_authority import emit_operator_initiated

    events = tmp_path / ".fno" / "events.jsonl"
    emit_operator_initiated("gate_set_operator_initiated", events_path=events, gate="quality_check_passed")
    emit_operator_initiated("gate_set_operator_initiated", events_path=events, gate="output_validated")
    lines = events.read_text().strip().splitlines()
    assert len(lines) == 2
    assert json.loads(lines[0])["data"]["gate"] == "quality_check_passed"
    # Unspecified source defaults to "target".
    assert json.loads(lines[1])["source"] == "target"


def test_emit_operator_initiated_swallows_write_errors(tmp_path: Path, capsys) -> None:
    from fno.agents.drive_authority import emit_operator_initiated

    # Point at a path whose parent cannot be created (a file occupies it) so the
    # OSError path is exercised; the call must not raise.
    blocker = tmp_path / "blocker"
    blocker.write_text("x")
    emit_operator_initiated("x_operator_initiated", events_path=blocker / "sub" / "events.jsonl")
    assert "emit_operator_initiated" in capsys.readouterr().err


# --- CLI verb --------------------------------------------------------------


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def test_cli_drive_authority_exit_codes_and_json(
    runner: CliRunner, monkeypatch, tmp_path: Path
) -> None:
    from fno import paths
    from fno.agents.cli import agents_app

    use_tmpdir(monkeypatch, tmp_path)
    agents_dir = paths.state_dir() / "agents"
    agents_dir.mkdir(parents=True, exist_ok=True)

    # No windows -> exit 1, "active": false.
    res = runner.invoke(agents_app, ["drive-authority", "--json"])
    assert res.exit_code == 1
    assert json.loads(res.output)["active"] is False

    # Open an interactive window -> exit 0, session reported.
    _write_state(agents_dir, "wkA", _drive(True, "interactive", "d-a"))
    res = runner.invoke(agents_app, ["drive-authority", "--json"])
    assert res.exit_code == 0
    payload = json.loads(res.output)
    assert payload["active"] is True
    assert payload["sessions"][0]["short_id"] == "wkA"
