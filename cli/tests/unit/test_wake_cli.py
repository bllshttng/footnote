"""Tests for fno wake CLI subapp (Task 2.3).

Tests exercise the top-level `app` so the `wake` subapp wiring is verified.
"""
from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app


runner = CliRunner()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _drop_two(tmp_path: Path) -> tuple[str, str]:
    """Drop two signals into tmp_path as the repo root. Returns (id1, id2)."""
    from fno.wake.signal import WakeSignal, drop_signal

    s1 = WakeSignal(
        source="test-source",
        kind="question",
        msg_id="msg-001",
        from_project="project-a",
        summary="first signal",
        ts=datetime(2026, 5, 5, 10, 0, tzinfo=timezone.utc),
    )
    s2 = WakeSignal(
        source="test-source",
        kind="lesson",
        msg_id="msg-002",
        from_project="project-b",
        summary="second signal",
        ts=datetime(2026, 5, 5, 11, 0, tzinfo=timezone.utc),
    )
    drop_signal(tmp_path, s1)
    drop_signal(tmp_path, s2)
    return s1.signal_id, s2.signal_id


# ---------------------------------------------------------------------------
# AC1-HP: wake list table mode
# ---------------------------------------------------------------------------

def test_list_table(tmp_path):
    """AC1-HP: drop two signals, list shows both signal_ids in table output."""
    id1, id2 = _drop_two(tmp_path)

    result = runner.invoke(
        app,
        ["wake", "list"],
        env={"FNO_WAKE_REPO_ROOT": str(tmp_path)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    assert id1 in result.output, f"signal_id {id1} missing from output"
    assert id2 in result.output, f"signal_id {id2} missing from output"


# ---------------------------------------------------------------------------
# AC1-HP-2: wake list --json
# ---------------------------------------------------------------------------

def test_list_json(tmp_path):
    """AC1-HP-2: wake list --json returns parseable array of length 2."""
    id1, id2 = _drop_two(tmp_path)

    result = runner.invoke(
        app,
        ["wake", "list", "--json"],
        env={"FNO_WAKE_REPO_ROOT": str(tmp_path)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    data = json.loads(result.output)
    assert isinstance(data, list), "Expected JSON array"
    assert len(data) == 2, f"Expected 2 items, got {len(data)}"
    ids_in_output = {item["signal_id"] for item in data}
    assert id1 in ids_in_output
    assert id2 in ids_in_output


# ---------------------------------------------------------------------------
# AC2-ERR: wake clear deletes all signals
# ---------------------------------------------------------------------------

def test_clear_deletes_all(tmp_path):
    """AC2-ERR: drop three signals, clear removes them all; output reports count."""
    from fno.wake.signal import WakeSignal, drop_signal

    for i in range(3):
        s = WakeSignal(
            source="test",
            kind="question",
            msg_id=f"msg-{i:03d}",
            from_project="proj",
            summary=f"signal {i}",
            ts=datetime(2026, 5, 5, 10, i, tzinfo=timezone.utc),
        )
        drop_signal(tmp_path, s)

    result = runner.invoke(
        app,
        ["wake", "clear"],
        env={"FNO_WAKE_REPO_ROOT": str(tmp_path)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    assert "cleared 3 signals" in result.output

    # Directory should be empty (or not exist, either acceptable)
    wake_dir = tmp_path / ".fno" / "wake-signals"
    if wake_dir.exists():
        remaining = list(wake_dir.glob("wake-*.json"))
        assert remaining == [], f"Expected empty wake-signals dir, found: {remaining}"


# ---------------------------------------------------------------------------
# AC4-EDGE: wake list with no wake-signals dir
# ---------------------------------------------------------------------------

def test_list_empty_no_dir(tmp_path):
    """AC4-EDGE: no wake-signals dir -> 'no signals', exit 0."""
    result = runner.invoke(
        app,
        ["wake", "list"],
        env={"FNO_WAKE_REPO_ROOT": str(tmp_path)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    assert "no signals" in result.output


def test_list_empty_json_no_dir(tmp_path):
    """AC4-EDGE: no wake-signals dir with --json -> '[]', exit 0."""
    result = runner.invoke(
        app,
        ["wake", "list", "--json"],
        env={"FNO_WAKE_REPO_ROOT": str(tmp_path)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    assert result.output.strip() == "[]"


# ---------------------------------------------------------------------------
# drop creates a signal file
# ---------------------------------------------------------------------------

def test_drop_creates_signal(tmp_path):
    """wake drop --source ... creates a file and prints the signal_id."""
    result = runner.invoke(
        app,
        [
            "wake", "drop",
            "--source", "test-source",
            "--kind", "question",
            "--msg-id", "msg-x01",
            "--from", "project-a",
            "--summary", "hello from test",
        ],
        env={"FNO_WAKE_REPO_ROOT": str(tmp_path)},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"Expected 0, got {result.exit_code}: {result.output}"
    signal_id = result.output.strip()
    assert signal_id.startswith("wake-"), f"Expected wake-XXXX id, got: {signal_id!r}"

    wake_dir = tmp_path / ".fno" / "wake-signals"
    expected_file = wake_dir / f"{signal_id}.json"
    assert expected_file.exists(), f"Signal file not found: {expected_file}"

    payload = json.loads(expected_file.read_text())
    assert payload["source"] == "test-source"
    assert payload["kind"] == "question"
    assert payload["msg_id"] == "msg-x01"
    assert payload["from_project"] == "project-a"
    assert payload["summary"] == "hello from test"
