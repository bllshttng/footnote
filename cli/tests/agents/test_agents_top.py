"""`fno agents top` (x-c5cc US4): union table, degradation, empty state, JSON parity."""
from __future__ import annotations

import json
import os

import pytest
from typer.testing import CliRunner

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
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/l",
            status="busy",
            pid=ALIVE,
            short_id="aaaa0000",
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
            harness="claude",
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
            harness="claude",
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


# --------------------------------------------------------------------------
# --subagents read-only sidechain section (x-af92)
# --------------------------------------------------------------------------
def _subagent_row(
    agent_id="af8f986001a0cc559",
    parent="62ec5501-9d77-4430-bc34-a2d036dbeb79",
    verdict="active",
    age=30.0,
):
    from fno.agents.discover import DiscoveredSubagent

    return DiscoveredSubagent(
        agent_id=agent_id,
        parent_session_id=parent,
        cwd="/Users/x/code/proj",
        git_branch="feature/x",
        transcript_path="/tmp/agent.jsonl",
        age_seconds=age,
        verdict=verdict,
    )


def _hermetic_registry(monkeypatch):
    """Keep census() off the operator's real registry/roster."""
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])


def test_subagents_flag_lists_sidechain_rows(monkeypatch, runner):
    """AC4-HP: --subagents renders agentId, parent, and an mtime verdict."""
    _hermetic_registry(monkeypatch)
    monkeypatch.setattr(
        "fno.agents.top.discover_subagents", lambda **kw: ([_subagent_row()], [])
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top", "--subagents"])
    assert result.exit_code == 0, result.output
    assert "af8f986001a0cc559" in result.output
    assert "62ec5501" in result.output  # parent session (short)
    assert "active" in result.output
    assert "600s" in result.output  # the live threshold is stated


def test_subagents_empty_reports_scope_not_none(monkeypatch, runner):
    """AC7-EDGE: no sidechain rows -> scope note, not a bare 'none running' read."""
    _hermetic_registry(monkeypatch)
    monkeypatch.setattr("fno.agents.top.discover_subagents", lambda **kw: ([], []))
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top", "--subagents"])
    assert result.exit_code == 0, result.output
    assert "claude only" in result.output
    assert "not measured" in result.output


def test_subagents_absent_from_default_top(monkeypatch, runner):
    """The sidechain section never runs without --subagents."""
    _hermetic_registry(monkeypatch)
    called = {"n": 0}

    def _spy(**kw):
        called["n"] += 1
        return [], []

    monkeypatch.setattr("fno.agents.top.discover_subagents", _spy)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top"])
    assert result.exit_code == 0
    assert called["n"] == 0
    assert "subagents (claude only" not in result.output


def test_subagents_json_emits_key(monkeypatch, runner):
    """AC4/parity: --subagents --json adds a subagents key with verdicts."""
    _hermetic_registry(monkeypatch)
    monkeypatch.setattr(
        "fno.agents.top.discover_subagents",
        lambda **kw: ([_subagent_row(verdict="idle", age=1200.0)], []),
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top", "--subagents", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert "subagents" in payload
    assert payload["subagents"][0]["agent_id"] == "af8f986001a0cc559"
    assert payload["subagents"][0]["verdict"] == "idle"
