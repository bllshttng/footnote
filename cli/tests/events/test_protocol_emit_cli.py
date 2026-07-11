"""Tests for `fno event emit` on the a2a status-breakpoint family (x-dbaf, US2).

Covers the emit-path auto-stamping: envelope coordinates from flags with
manifest fallback, identity omitted for a non-session (bare-shell) producer,
and the run/node fallback to the manifest.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.events.cli import cli as event_cli


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def _events_path(tmp_path: Path) -> Path:
    return tmp_path / ".fno" / "events.jsonl"


def _last_event(events: Path) -> dict:
    lines = [ln for ln in events.read_text().splitlines() if ln.strip()]
    return json.loads(lines[-1])


def _no_session(monkeypatch) -> None:
    # Force the "non-session producer" path regardless of the test host's
    # ambient harness markers (this suite may run inside a live claude session).
    from fno.harness_identity import HarnessIdentity

    monkeypatch.setattr(
        "fno.harness_identity.resolve_harness_identity",
        lambda *a, **k: HarnessIdentity(session_id=None, harness=None),
    )


def _emit(runner, tmp_path, monkeypatch, *args):
    _no_session(monkeypatch)
    events = _events_path(tmp_path)
    state = tmp_path / ".fno" / "target-state.md"  # absent by default
    return runner.invoke(
        event_cli,
        ["emit", "--events", str(events), "--state", str(state), *args],
    )


# -- AC1-EDGE: non-session producer omits from/model entirely --

def test_non_session_producer_omits_identity(runner, tmp_path, monkeypatch) -> None:
    result = _emit(
        runner, tmp_path, monkeypatch,
        "--type", "task_started",
        "--source", "test",
        "--run", "tgt-run-9",
        "--data", '{"title": "t"}',
    )
    assert result.exit_code == 0, result.output
    ev = _last_event(_events_path(tmp_path))
    assert ev["type"] == "task_started"
    assert ev["v"] == 1
    assert ev["run"] == "tgt-run-9"
    assert "from" not in ev  # omitted, not empty string
    assert "model" not in ev


# -- manifest fallback for run/node when flags omitted --

def test_run_and_node_fall_back_to_manifest(runner, tmp_path, monkeypatch) -> None:
    state = tmp_path / ".fno" / "target-state.md"
    state.parent.mkdir(parents=True, exist_ok=True)
    state.write_text(
        "---\nsession_id: 20260711T120000Z-abc-123\n---\n"
        "graph_node_id: prj-0001\n"
    )
    events = _events_path(tmp_path)
    _no_session(monkeypatch)
    result = runner.invoke(
        event_cli,
        [
            "emit", "--events", str(events), "--state", str(state),
            "--type", "task_started", "--source", "test", "--data", "{}",
        ],
    )
    assert result.exit_code == 0, result.output
    ev = _last_event(events)
    assert ev["run"] == "20260711T120000Z-abc-123"  # from manifest session_id
    assert ev["node"] == "prj-0001"  # from manifest graph_node_id


# -- explicit flags win over manifest; outcome carried on task_done --

def test_task_done_with_flags(runner, tmp_path, monkeypatch) -> None:
    result = _emit(
        runner, tmp_path, monkeypatch,
        "--type", "task_done",
        "--source", "test",
        "--run", "tgt-run-1",
        "--node", "prj-0002",
        "--task", "2.1",
        "--outcome", "SUCCESS",
        "--data", '{"commit": "abc123"}',
    )
    assert result.exit_code == 0, result.output
    ev = _last_event(_events_path(tmp_path))
    assert ev["task"] == "2.1"
    assert ev["node"] == "prj-0002"
    assert ev["outcome"] == "SUCCESS"


# -- AC2-EDGE: oversized data string truncated to the cap, event still written --

def test_blocked_reason_truncated_to_cap(runner, tmp_path, monkeypatch) -> None:
    result = _emit(
        runner, tmp_path, monkeypatch,
        "--type", "blocked",
        "--source", "test",
        "--run", "tgt-run-1",
        "--data", json.dumps({"reason": "x" * 900}),
    )
    assert result.exit_code == 0, result.output
    ev = _last_event(_events_path(tmp_path))
    assert len(ev["data"]["reason"]) == 500  # truncated to the documented cap


# -- a bad outcome is rejected pre-lock (nothing appended) --

def test_bad_outcome_rejected_pre_lock(runner, tmp_path, monkeypatch) -> None:
    result = _emit(
        runner, tmp_path, monkeypatch,
        "--type", "task_done",
        "--source", "test",
        "--run", "tgt-run-1",
        "--outcome", "PARTIAL",
        "--data", "{}",
    )
    assert result.exit_code == 1
    assert not _events_path(tmp_path).exists()  # pre-lock reject: nothing written
