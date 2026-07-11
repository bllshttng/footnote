"""Push leg for the a2a status-breakpoint family (x-dbaf, US4).

blocked + run_summary notify the parent handle when spawn lineage exists; the
push rides `fno mail send` (durable-first), fires AFTER the durable events.jsonl
append, and silently skips when there is no lineage.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.events.cli import _resolve_parent_handle
from fno.events.cli import cli as event_cli


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


class _R:
    def __init__(self, rc=0):
        self.returncode = rc
        self.stdout = b"msg-1 queued (durable)"
        self.stderr = b""


def _emit_blocked(runner, tmp_path, monkeypatch, *, parent, run_fn):
    events = tmp_path / ".fno" / "events.jsonl"
    state = tmp_path / ".fno" / "target-state.md"
    monkeypatch.setattr("fno.events.cli._resolve_parent_handle", lambda explicit: parent)
    monkeypatch.setattr("fno.events.cli.subprocess.run", run_fn)
    result = runner.invoke(
        event_cli,
        ["emit", "--events", str(events), "--state", str(state),
         "--type", "blocked", "--source", "test", "--run", "R1",
         "--data", json.dumps({"reason": "stuck on x"})],
    )
    return result, events


# -- resolution --

def test_resolve_parent_explicit_wins() -> None:
    assert _resolve_parent_handle("claude-parent99") == "claude-parent99"


# -- AC2-HP: blocked with lineage pushes to the parent, referencing the run --

def test_blocked_pushes_to_parent(runner, tmp_path, monkeypatch) -> None:
    sent: dict = {}

    def fake_run(argv, **kw):
        sent["argv"] = argv
        return _R(0)

    result, events = _emit_blocked(runner, tmp_path, monkeypatch, parent="claude-parent99", run_fn=fake_run)
    assert result.exit_code == 0, result.output
    ev = json.loads(events.read_text().splitlines()[-1])
    assert ev["type"] == "blocked"
    assert sent["argv"][:3] == ["fno", "mail", "send"]
    assert "claude-parent99" in sent["argv"]
    # message references the run so the parent can correlate
    assert any("R1" in a for a in sent["argv"])


# -- no lineage -> silent skip, no mail send --

def test_no_parent_no_push(runner, tmp_path, monkeypatch) -> None:
    calls: list = []

    def fake_run(argv, **kw):
        calls.append(argv)
        return _R(0)

    result, events = _emit_blocked(runner, tmp_path, monkeypatch, parent=None, run_fn=fake_run)
    assert result.exit_code == 0, result.output
    assert events.exists()  # event still written
    assert not any(a[:3] == ["fno", "mail", "send"] for a in calls)


# -- AC1-FR: a failing push loses nothing (event already durable, exit 0) --

def test_push_failure_keeps_event(runner, tmp_path, monkeypatch) -> None:
    def boom(argv, **kw):
        raise OSError("mail bus down")

    result, events = _emit_blocked(runner, tmp_path, monkeypatch, parent="claude-parent99", run_fn=boom)
    assert result.exit_code == 0, result.output  # push failure is non-fatal
    ev = json.loads(events.read_text().splitlines()[-1])
    assert ev["type"] == "blocked"  # events.jsonl line intact, independent of push


# -- push-parent subcommand: skip without lineage --

def test_push_parent_subcommand_skips(runner, monkeypatch) -> None:
    monkeypatch.setattr("fno.events.cli._resolve_parent_handle", lambda explicit: None)
    result = runner.invoke(event_cli, ["push-parent", "--type", "run_summary", "--run", "R1"])
    assert result.exit_code == 0
    assert "no parent lineage" in result.output


def test_push_parent_subcommand_pushes(runner, monkeypatch) -> None:
    monkeypatch.setattr("fno.events.cli._resolve_parent_handle", lambda explicit: "claude-parent99")
    sent: dict = {}
    monkeypatch.setattr(
        "fno.events.cli.subprocess.run",
        lambda argv, **kw: (sent.__setitem__("argv", argv), _R(0))[1],
    )
    result = runner.invoke(
        event_cli,
        ["push-parent", "--type", "run_summary", "--run", "R1", "--reason", "DonePRGreen"],
    )
    assert result.exit_code == 0
    assert sent["argv"][:3] == ["fno", "mail", "send"]
    assert "claude-parent99" in sent["argv"]
