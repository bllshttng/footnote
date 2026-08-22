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


# ---------------------------------------------------------------------------
# --pane-stats: per-pane mux counter deltas, one reader for every consumer
# ---------------------------------------------------------------------------


def _counter_event(tmp_path, monkeypatch, rows_by_ts):
    """Write mux_pane_counters snapshots to a pinned journal and point
    pane_counter_rows at it. rows_by_ts: {ts: [pane rows]}."""
    journal = tmp_path / "global-events.jsonl"
    with journal.open("a") as fh:
        for ts, panes in rows_by_ts.items():
            fh.write(
                json.dumps(
                    {"ts": ts, "type": "mux_pane_counters", "source": "daemon",
                     "data": {"session": "main", "panes": panes}}
                )
                + "\n"
            )
    return journal


_PANE_A = {
    "pane_id": 3, "node": "x-deadbeef", "name": "peer", "cmd": "claude",
    "bytes_in": 100, "grid_updates": 10, "frames_composited": 6,
    "frames_emitted": 4, "cpu_ns": 1_000_000_000,
}


def test_pane_stats_differences_last_two_samples(tmp_path, monkeypatch):
    from fno.agents.top import pane_counter_rows

    journal = _counter_event(
        tmp_path,
        monkeypatch,
        {
            "2026-08-22T14:30:00Z": [_PANE_A],
            "2026-08-22T14:30:30Z": [{**_PANE_A, "bytes_in": 350, "grid_updates": 33,
                                      "frames_composited": 20, "frames_emitted": 13,
                                      "cpu_ns": 2_500_000_000}],
        },
    )
    section = pane_counter_rows(journal)
    assert section["status"] == "ok"
    assert section["window_s"] == 30.0
    assert len(section["rows"]) == 1
    row = section["rows"][0]
    assert row["pane_id"] == 3
    assert row["node"] == "x-deadbeef"
    assert row["bytes_in"] == 250
    assert row["grid_updates"] == 23
    assert row["frames_composited"] == 14
    assert row["frames_emitted"] == 9
    assert row["cpu_ns"] == 1_500_000_000


def test_pane_stats_reports_born_and_gone(tmp_path, monkeypatch):
    from fno.agents.top import pane_counter_rows

    pane_b = {**_PANE_A, "pane_id": 4, "bytes_in": 5}
    journal = _counter_event(
        tmp_path,
        monkeypatch,
        {
            "2026-08-22T14:30:00Z": [_PANE_A],
            "2026-08-22T14:30:30Z": [pane_b],
        },
    )
    section = pane_counter_rows(journal)
    assert section["status"] == "ok"
    assert section["rows"] == []  # no pane appeared in both samples
    assert section["born"] == [4]
    assert section["gone"] == [3]


def test_pane_stats_single_sample_says_so(tmp_path, monkeypatch):
    """Honest-edge: one sample prints an explicit insufficiency, never an
    empty table that reads as 'no cost'."""
    from fno.agents.top import _render_pane_stats_lines, pane_counter_rows

    journal = _counter_event(
        tmp_path, monkeypatch, {"2026-08-22T14:30:00Z": [_PANE_A]}
    )
    section = pane_counter_rows(journal)
    assert section["status"] == "insufficient-samples"
    lines = _render_pane_stats_lines(section)
    assert any("insufficient samples" in line for line in lines)


def test_pane_stats_missing_journal_is_insufficient_not_error(tmp_path, monkeypatch):
    from fno.agents.top import pane_counter_rows

    section = pane_counter_rows(tmp_path / "nope.jsonl")
    assert section["status"] == "insufficient-samples"


def test_pane_stats_restart_resets_instead_of_differencing(tmp_path, monkeypatch):
    """A mux restart reuses pane ids from 1 with zeroed totals; differencing
    across the reset would fabricate negative deltas. Session change reports
    born/gone instead."""
    from fno.agents.top import pane_counter_rows

    fresh = {**_PANE_A, "bytes_in": 2, "grid_updates": 1, "frames_composited": 1,
             "frames_emitted": 1, "cpu_ns": 1000}
    journal = tmp_path / "global-events.jsonl"
    with journal.open("a") as fh:
        for ts, session, panes in [
            ("2026-08-22T14:30:00Z", "main", [_PANE_A]),
            ("2026-08-22T14:30:30Z", "restarted", [fresh]),
        ]:
            fh.write(
                json.dumps(
                    {"ts": ts, "type": "mux_pane_counters", "source": "daemon",
                     "data": {"session": session, "panes": panes}}
                )
                + "\n"
            )
    section = pane_counter_rows(journal)
    assert section["status"] == "ok"
    assert section["rows"] == []
    assert section["born"] == [3]
    assert section["gone"] == [3]


def test_pane_stats_flag_renders_into_top(monkeypatch, runner):
    _hermetic_registry(monkeypatch)
    monkeypatch.setattr(
        "fno.agents.top.pane_counter_rows",
        lambda *a, **kw: {
            "status": "ok",
            "rows": [
                {"pane_id": 3, "node": "x-deadbeef", "name": "peer", "cmd": "claude",
                 "bytes_in": 250, "grid_updates": 23, "frames_composited": 14,
                 "frames_emitted": 9, "cpu_ns": 1_500_000_000}
            ],
            "born": [],
            "gone": [],
            "session": "main",
            "window_s": 30.0,
        },
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top", "--pane-stats"])
    assert result.exit_code == 0, result.output
    assert "pane counters" in result.output
    assert "x-deadbeef" in result.output
    # The flag-off default stays clean.
    result_off = runner.invoke(agents_app, ["top"])
    assert "pane counters" not in result_off.output
