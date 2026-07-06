"""`fno agents top` (x-c5cc US4): union table, degradation, empty state, JSON parity."""
from __future__ import annotations

import json
import os

import pytest
from typer.testing import CliRunner

from fno.agents import spawn_gate
from fno.agents.registry import AgentEntry


@pytest.fixture(autouse=True)
def _isolated_world(tmp_path, monkeypatch):
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims-root"))
    yield


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


ALIVE = os.getpid()


def _seed(tmp_path, monkeypatch):
    """One fno row + one foreign roster worker, both alive."""
    roster = {
        "proto": 1,
        "workers": {"7c5dcf5d": {"sessionId": "7c5dcf5d-1-2-3-4", "pid": ALIVE}},
    }
    (tmp_path / "daemon" / "roster.json").write_text(json.dumps(roster))
    rows = [
        AgentEntry(
            name="think-x1",
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/l",
            status="busy",
            pid=ALIVE,
            claude_short_id="aaaa0000",
        )
    ]
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)


def test_union_table_marks_foreign_rows(tmp_path, monkeypatch, runner):
    """AC4-HP: both sources render; foreign rows marked."""
    _seed(tmp_path, monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top"])
    assert result.exit_code == 0, result.output
    assert "think-x1" in result.output
    assert "7c5dcf5d" in result.output
    assert "(foreign)" in result.output
    assert "RSS_MB" in result.output


def test_empty_state_is_explicit(monkeypatch, runner):
    """AC4-UI: no live workers -> an explicit line, not a bare table."""
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top"])
    assert result.exit_code == 0
    assert "no live workers" in result.output


def test_malformed_roster_degrades_per_source(tmp_path, monkeypatch, runner):
    """AC4-ERR: fno rows still render; the claude failure is noted; exit 0."""
    (tmp_path / "daemon" / "roster.json").write_text("{ nope")
    rows = [
        AgentEntry(
            name="ok-worker",
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/l",
            status="idle",
            pid=ALIVE,
        )
    ]
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top"])
    assert result.exit_code == 0
    assert "ok-worker" in result.output
    assert "roster unreadable" in result.output


def test_dead_pids_excluded(monkeypatch, runner):
    """AC4-EDGE: a `live` row with a dead pid does not render as live."""
    rows = [
        AgentEntry(
            name="ghost",
            provider="claude",
            cwd="/tmp",
            log_path="/tmp/l",
            status="live",
            pid=4194321,
        )
    ]
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top"])
    assert "ghost" not in result.output


def test_json_parity(tmp_path, monkeypatch, runner):
    """AC4-FR: --json emits the same rows the table shows."""
    _seed(tmp_path, monkeypatch)
    from fno.agents.cli import agents_app

    table = runner.invoke(agents_app, ["top"])
    as_json = runner.invoke(agents_app, ["top", "--json"])
    assert as_json.exit_code == 0
    payload = json.loads(as_json.output)
    names = {w["name"] for w in payload["workers"]}
    assert names == {"think-x1", "7c5dcf5d"}
    for name in names:
        assert name in table.output
