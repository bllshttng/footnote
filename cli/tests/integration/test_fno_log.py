"""Integration tests for 'fno log' subcommand and agent_progress reader.

Tests cover:
- Each of the 4 log commands (activity, milestone, warning, user_note)
- Session ID resolution (from env vs fallback)
- --details JSON parsing
- Write failure exits non-zero
- Reader helpers: read_progress, latest_entry, newest_ts_within
"""
from __future__ import annotations

import json
import os
from pathlib import Path
from unittest.mock import patch, mock_open

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def _read_progress_file(tmp_path: Path) -> list[dict]:
    progress_file = tmp_path / ".fno" / "agent-progress.jsonl"
    assert progress_file.exists(), f"Expected progress file at {progress_file}"
    lines = progress_file.read_text(encoding="utf-8").splitlines()
    return [json.loads(line) for line in lines if line.strip()]


def test_activity_appends_entry(tmp_path: Path, monkeypatch):
    """AC1-HP: activity command writes kind=activity entry."""
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["log", "activity", "doing X"])
    assert result.exit_code == 0, f"Expected exit 0, got {result.exit_code}: {result.output}"
    entries = _read_progress_file(tmp_path)
    assert len(entries) == 1
    assert entries[0]["kind"] == "activity"
    assert entries[0]["summary"] == "doing X"


def test_milestone_appends_entry(tmp_path: Path, monkeypatch):
    """AC1-HP: milestone command writes kind=milestone entry."""
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["log", "milestone", "phase complete"])
    assert result.exit_code == 0, f"Expected exit 0, got {result.exit_code}: {result.output}"
    entries = _read_progress_file(tmp_path)
    assert len(entries) == 1
    assert entries[0]["kind"] == "milestone"
    assert entries[0]["summary"] == "phase complete"


def test_warning_appends_entry(tmp_path: Path, monkeypatch):
    """AC1-HP: warning command writes kind=warning entry."""
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["log", "warning", "something is off"])
    assert result.exit_code == 0, f"Expected exit 0, got {result.exit_code}: {result.output}"
    entries = _read_progress_file(tmp_path)
    assert len(entries) == 1
    assert entries[0]["kind"] == "warning"
    assert entries[0]["summary"] == "something is off"


def test_user_note_appends_entry(tmp_path: Path, monkeypatch):
    """AC1-HP: user_note command writes kind=user_note entry, session_id starts with 'manual-'."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    result = runner.invoke(app, ["log", "user-note", "a human note"])
    assert result.exit_code == 0, f"Expected exit 0, got {result.exit_code}: {result.output}"
    entries = _read_progress_file(tmp_path)
    assert len(entries) == 1
    assert entries[0]["kind"] == "user_note"
    assert entries[0]["summary"] == "a human note"
    assert entries[0]["session_id"].startswith("manual-")


def test_session_id_from_env(tmp_path: Path, monkeypatch):
    """AC1-HP: when CLAUDECODE_SESSION_ID is set, entry uses it as session_id."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "abc123")
    result = runner.invoke(app, ["log", "activity", "task running"])
    assert result.exit_code == 0, f"Expected exit 0, got {result.exit_code}: {result.output}"
    entries = _read_progress_file(tmp_path)
    assert entries[0]["session_id"] == "abc123"


def test_session_id_fallback(tmp_path: Path, monkeypatch):
    """AC1-HP: without CLAUDECODE_SESSION_ID, session_id starts with 'manual-'."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    result = runner.invoke(app, ["log", "activity", "local run"])
    assert result.exit_code == 0, f"Expected exit 0, got {result.exit_code}: {result.output}"
    entries = _read_progress_file(tmp_path)
    assert entries[0]["session_id"].startswith("manual-")


def test_details_object_preserved(tmp_path: Path, monkeypatch):
    """AC1-HP: --details JSON string is parsed and stored as object in entry."""
    monkeypatch.chdir(tmp_path)
    details_json = '{"file":"foo.py","line":42}'
    result = runner.invoke(app, ["log", "activity", "X", "--details", details_json])
    assert result.exit_code == 0, f"Expected exit 0, got {result.exit_code}: {result.output}"
    entries = _read_progress_file(tmp_path)
    assert entries[0]["details"] == {"file": "foo.py", "line": 42}


def test_write_failure_exits_nonzero(tmp_path: Path, monkeypatch):
    """AC4-ERR: when write fails (OS error), command prints to stderr and exits with code 2."""
    monkeypatch.chdir(tmp_path)
    # Patch the _emit function in log_cmd to simulate an OSError on write.
    # We patch Path.open via the io module since Path.open is read-only in CPython 3.13.
    import pathlib
    from unittest.mock import patch

    original_path_open = pathlib.Path.open

    def failing_path_open(self, mode="r", *args, **kwargs):
        if "agent-progress.jsonl" in str(self) and ("a" in mode or "w" in mode):
            raise OSError("No space left on device")
        return original_path_open(self, mode, *args, **kwargs)

    with patch.object(pathlib.Path, "open", failing_path_open):
        result = runner.invoke(app, ["log", "activity", "will fail"])

    assert result.exit_code == 2, f"Expected exit 2, got {result.exit_code}: {result.output}"


def test_read_progress_returns_last_n(tmp_path: Path):
    """AC1-HP: read_progress(last_n=3) returns 3 most recent of 5 entries."""
    from fno.agent_progress import read_progress

    abilities_dir = tmp_path / ".fno"
    abilities_dir.mkdir()
    progress_file = abilities_dir / "agent-progress.jsonl"

    entries = [
        {"ts": f"2026-04-29T0{i}:00:00Z", "session_id": "test", "kind": "activity", "summary": f"entry {i}"}
        for i in range(5)
    ]
    progress_file.write_text("\n".join(json.dumps(e) for e in entries) + "\n", encoding="utf-8")

    result = read_progress(tmp_path, last_n=3)
    assert len(result) == 3
    assert result[0]["summary"] == "entry 2"
    assert result[1]["summary"] == "entry 3"
    assert result[2]["summary"] == "entry 4"


def test_read_progress_empty_when_missing(tmp_path: Path):
    """AC1-HP: read_progress returns [] when progress file doesn't exist."""
    from fno.agent_progress import read_progress

    # tmp_path has no .fno dir
    result = read_progress(tmp_path)
    assert result == []
