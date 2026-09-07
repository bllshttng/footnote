"""`fno agents top` (x-c5cc US4): union table, degradation, empty state, JSON parity.

Read-only over the registry, so it emits nothing; the conftest per-module pin
sets FNO_EVENTS_PATH to a per-test tmp journal regardless.
"""
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


def test_pane_stats_reads_rows_from_the_ephemeral_sibling(tmp_path, monkeypatch):
    """Post-routing shape: the gauge's rows live only in the .ephemeral sibling."""
    from fno.agents.top import pane_counter_rows

    journal = _counter_event(
        tmp_path,
        monkeypatch,
        {
            "2026-08-22T14:30:00Z": [_PANE_A],
            "2026-08-22T14:30:30Z": [{**_PANE_A, "bytes_in": 250}],
        },
    )
    sibling = journal.with_name(journal.name + ".ephemeral")
    sibling.write_text(journal.read_text())
    journal.unlink()  # post-deploy: the durable journal holds no gauge rows

    section = pane_counter_rows(journal)
    assert section["status"] == "ok"
    assert len(section["rows"]) == 1
    assert section["rows"][0]["bytes_in"] == 150


def test_pane_stats_bridges_a_just_rotated_sibling(tmp_path, monkeypatch):
    """A sibling that rotated mid-window holds its newest pair in the .1
    generation; the reader must bridge instead of reporting
    insufficient-samples for the active file's first minute."""
    from fno.agents.top import pane_counter_rows

    journal = tmp_path / "global-events.jsonl"
    older_gen = journal.with_name(journal.name + ".ephemeral.1")
    with older_gen.open("a") as fh:
        for ts, pane in (
            ("2026-08-22T14:30:00Z", _PANE_A),
            ("2026-08-22T14:30:30Z", {**_PANE_A, "bytes_in": 250}),
        ):
            fh.write(
                json.dumps(
                    {"ts": ts, "type": "mux_pane_counters", "source": "daemon",
                     "data": {"session": "main", "panes": [pane]}}
                )
                + "\n"
            )
    active_sibling = journal.with_name(journal.name + ".ephemeral")
    active_sibling.write_text("")  # just rotated: empty until the next 30s tick

    section = pane_counter_rows(journal)
    assert section["status"] == "ok"
    assert section["rows"][0]["bytes_in"] == 150


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
    """A mux restart under a NEW session name reuses pane ids from 1 with
    zeroed totals; the session grouping differences within one session only,
    so the rename reports every pane as born/gone instead."""
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
    # The renamed session has one sample only: honest insufficiency, not a
    # delta fabricated across the rename.
    assert section["status"] == "insufficient-samples"
    assert section["rows"] == []


def test_pane_stats_same_socket_reset_reads_as_reset_not_negative_delta(tmp_path, monkeypatch):
    """A server restarting on the SAME socket name keeps the session label
    while pane ids and totals reset; a decreased total must report the pane
    born-and-gone, never a negative delta."""
    from fno.agents.top import pane_counter_rows

    reset = {**_PANE_A, "bytes_in": 1, "grid_updates": 0, "frames_composited": 0,
             "frames_emitted": 0, "cpu_ns": 5}
    journal = _counter_event(
        tmp_path,
        monkeypatch,
        {
            "2026-08-22T14:30:00Z": [_PANE_A],
            "2026-08-22T14:30:30Z": [reset],
        },
    )
    section = pane_counter_rows(journal)
    assert section["status"] == "ok"
    assert section["rows"] == []
    assert section["born"] == [3]
    assert section["gone"] == [3]


def test_pane_stats_differences_within_one_session_when_journals_interleave(
    tmp_path, monkeypatch,
):
    """Two live mux sessions append interleaved rows to one journal; the last
    two rows journal-wide can belong to different sessions. The reader groups
    by session and differences the journal-latest session's own pair."""
    from fno.agents.top import pane_counter_rows

    journal = tmp_path / "global-events.jsonl"
    rows = [
        ("2026-08-22T14:30:00Z", "main", [_PANE_A]),
        ("2026-08-22T14:30:15Z", "scratch", [{**_PANE_A, "pane_id": 9, "bytes_in": 50}]),
        ("2026-08-22T14:30:30Z", "main", [{**_PANE_A, "bytes_in": 350}]),
    ]
    with journal.open("a") as fh:
        for ts, session, panes in rows:
            fh.write(
                json.dumps(
                    {"ts": ts, "type": "mux_pane_counters", "source": "daemon",
                     "data": {"session": session, "panes": panes}}
                )
                + "\n"
            )
    section = pane_counter_rows(journal)
    assert section["status"] == "ok"
    assert section["session"] == "main"
    assert len(section["rows"]) == 1
    assert section["rows"][0]["pane_id"] == 3
    assert section["rows"][0]["bytes_in"] == 250


def test_pane_stats_skips_rows_without_integer_pane_id(tmp_path, monkeypatch):
    """A pane row missing pane_id is journal noise; the debug view must skip
    it and keep rendering, never KeyError the whole table."""
    from fno.agents.top import pane_counter_rows

    journal = tmp_path / "global-events.jsonl"
    with journal.open("a") as fh:
        for ts, panes in [
            ("2026-08-22T14:30:00Z", [{"noise": True}, _PANE_A]),
            ("2026-08-22T14:30:30Z", [{}, {**_PANE_A, "bytes_in": 400}]),
        ]:
            fh.write(
                json.dumps(
                    {"ts": ts, "type": "mux_pane_counters", "source": "daemon",
                     "data": {"session": "main", "panes": panes}}
                )
                + "\n"
            )
    section = pane_counter_rows(journal)
    assert section["status"] == "ok"
    assert len(section["rows"]) == 1
    assert section["rows"][0]["pane_id"] == 3
    assert section["rows"][0]["bytes_in"] == 300


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


# -- LANES: the cap that actually refuses --
#
# Measured 2026-09-01: agents.provider_limits.zai.lanes = 7 was binding (7 live
# rows, a spawn refused there) while machine load sat at 1.3 per CPU, far under
# the max_load_per_cpu = 8 trigger. Every machine-capacity surface said "plenty
# of room" and none of them was the thing saying no.


def _lane_world(monkeypatch, *, count=7, cap=7, holders=("w1", "w2"), raises=None):
    """Pin the two gate functions the lane block reads, and nothing else."""
    from fno.agents import top as top_mod
    from fno.agents.spawn_gate import ProviderCountUnavailable

    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])

    class _Budget:
        lanes = cap

    monkeypatch.setattr(
        "fno.config.provider_limits_table", lambda agents: {"zai": _Budget()}
    )
    monkeypatch.setattr(
        "fno.agents.spawn_gate.provider_lanes_cap", lambda budget: cap
    )

    def _count(provider, counted=None):
        if raises:
            raise ProviderCountUnavailable(raises)
        if counted is not None:
            counted.update(holders)
        return count

    monkeypatch.setattr("fno.agents.spawn_gate.provider_live_count", _count)
    return top_mod


def test_a_full_provider_lane_reads_full_with_its_holders(monkeypatch, runner):
    """AC8-HP. The binding cap, named, counted, and attributed."""
    _lane_world(monkeypatch, count=7, cap=7, holders=("t-a879", "t-reaper"))
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top"])
    assert result.exit_code == 0, result.output
    assert "LANES  zai" in result.output
    assert "7/7" in result.output
    assert "FULL" in result.output
    assert "t-a879" in result.output and "t-reaper" in result.output


def test_lane_holders_come_from_the_counter_not_a_second_walk(monkeypatch, runner):
    """The display and the refusal must read one population.

    A naive registry walk listed five openai rows beside the gate's count of 0,
    because `status == live` and positive liveness are different populations.
    Holders now come from the counter's own tally, so a row the count excluded
    can never be printed as though it occupied a lane.
    """
    _lane_world(monkeypatch, count=0, cap=7, holders=())
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top"])
    assert result.exit_code == 0, result.output
    assert "0/7" in result.output
    assert "holders:" not in result.output


def test_an_unreadable_lane_count_is_never_rendered_as_zero(monkeypatch, runner):
    """AC10-ERR. Zero free lanes and an unreadable registry are opposite facts.

    The gate itself treats unreadable as a refusal (fail-closed), so printing it
    as an empty fleet would invert the meaning.
    """
    _lane_world(monkeypatch, cap=7, raises="registry forward read skipped rows")
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top"])
    assert result.exit_code == 0, result.output
    assert "?/7" in result.output
    assert "unreadable" in result.output
    assert "0/7" not in result.output
    # The process table below still renders.
    assert "SOURCE" in result.output


def test_lanes_ride_in_json(monkeypatch, runner):
    """Script parity: the same facts, same names."""
    _lane_world(monkeypatch, count=7, cap=7, holders=("t-a879",))
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    lanes = {row["provider"]: row for row in payload["lanes"]}
    assert lanes["zai"]["count"] == 7
    assert lanes["zai"]["cap"] == 7
    assert lanes["zai"]["full"] is True
    assert lanes["zai"]["holders"] == ["t-a879"]


def test_the_unattributed_row_warning_fires_once_per_process(monkeypatch):
    """It describes the registry, not one provider, so asking about three
    providers used to print it three times."""
    from fno.agents import spawn_gate

    class _Row:
        status = "live"
        provider = None
        harness = "claude"
        origin = "adopted"
        name = "w"
        pid = None
        mux = None
        short_id = None

    monkeypatch.setattr(spawn_gate, "_UNATTRIBUTED_WARNED", set())
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [_Row()])
    monkeypatch.setattr(spawn_gate, "_provider_live_slot_claims", lambda p, n: 0)

    seen: list[str] = []
    monkeypatch.setattr(spawn_gate, "_warn", lambda m: seen.append(m))

    spawn_gate.provider_live_count("zai")
    spawn_gate.provider_live_count("anthropic")
    spawn_gate.provider_live_count("openai")

    assert len([m for m in seen if "without a provider stamp" in m]) == 1


def test_census_caption_points_at_the_transcript_verdict(runner):
    """The table's footer names itself a census and defers liveness to truth.

    A row's presence is launch-time evidence; the caption must say so in every
    render, including the empty board, so nobody reads STATUS as an answer
    about whether the session behind the row can move now.
    """
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top"])
    out = result.output
    assert result.exit_code == 0, out
    assert "census: PID/RSS are the process at scan time" in out, out
    assert "REACH reads the transcript" in out, out
    assert "fno agents truth" in out, out


def test_status_column_renders_served_activity_with_age(tmp_path, monkeypatch, runner):
    """AC7-HP (x-c672): the STATUS column answers what the session is doing -
    `writing 30s` for a transcript touched half a minute ago, `quiet 3h` for
    one three hours stale - and the word `live` appears in neither row."""
    roster = {"proto": 1, "workers": {}}
    (tmp_path / "daemon" / "roster.json").write_text(json.dumps(roster))
    rows = [
        AgentEntry(
            name="fresh-worker",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/l",
            status="busy",
            pid=ALIVE,
            short_id="aaaa0000",
        ),
        AgentEntry(
            name="stale-worker",
            harness="claude",
            cwd="/tmp",
            log_path="/tmp/m",
            status="busy",
            pid=ALIVE,
            short_id="bbbb1111",
        ),
    ]
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)

    from fno.agents import session_truth

    def fake_truth(handle, **_kwargs):
        fresh = handle == "fresh-worker"
        return {
            "state": "working",
            "last_activity_age_s": 30 if fresh else 3 * 3600,
        }

    monkeypatch.setattr(session_truth, "resolve_session_truth", fake_truth)

    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["top"])
    assert result.exit_code == 0, result.output
    assert "writing 30s" in result.output, result.output
    assert "quiet 3h" in result.output, result.output
    for line in result.output.splitlines():
        if "fresh-worker" in line or "stale-worker" in line:
            assert " live" not in f" {line}", line
