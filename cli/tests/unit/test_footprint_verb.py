"""Tests for the hidden ``fno doctor footprint`` diagnostic."""

from __future__ import annotations

import json
import os
import subprocess
import sys

import pytest
import typer
from typer.testing import CliRunner

from fno.footprint import parse_footprint
from fno.cli import app

# Import the mux_spawn -> dispatch chain at collection, before any test
# patches anything: patching `fno.agents.registry.load_registry` first and
# importing mux_spawn later would bind the patched loader into dispatch's
# `from registry import load_registry` permanently, poisoning every later
# test in the process. One module-level import pins that order for the file.
import fno.agents.mux_spawn  # noqa: F401,E402


runner = CliRunner()


@pytest.fixture
def no_worker_roots(monkeypatch):
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint,
        "_live_root_pids",
        lambda **_kwargs: (set(), None),
    )
    monkeypatch.setattr(
        doctor_footprint,
        "_live_shared_serve_root_pids",
        lambda **_kwargs: (set(), None),
    )


def _fake_runner(
    monkeypatch, ps_output: str, roster: list[dict], calls: list[list[str]]
):
    """Fake the ``ps`` snapshot and pin the live roster the count reads.

    The roster used to arrive over a ``fno agents list`` subprocess and now
    comes from an in-process registry read, so the fake moves with it. A
    subprocess other than ``ps`` is an assertion failure rather than a
    silently faked roster: that is what proves the shell-out is gone.
    """
    from types import SimpleNamespace

    monkeypatch.setattr(
        "fno.agents.registry.load_registry",
        lambda: [SimpleNamespace(status="live", **row) for row in roster],
    )

    def run(argv, **kwargs):
        calls.append(list(argv))
        if argv[0] == "ps":
            kwargs["stdout"].write(ps_output)
            return subprocess.CompletedProcess(argv, 0)
        raise AssertionError(f"unexpected subprocess in a footprint run: {argv}")

    return run


def _pin_load(monkeypatch, *, status: str, load: float = 1.0, ceiling: float = 96.0):
    """Pin the spawn-load snapshot so a verdict test is hermetic: the real
    snapshot reads the host's live load average, which no exit-code assertion
    should ride on."""
    from types import SimpleNamespace

    from fno import doctor_footprint

    snapshot = SimpleNamespace(
        load_1m=load,
        max_load_per_cpu=8.0,
        load_ceiling=ceiling,
        load_cpu_count=int(ceiling // 8),
        spawn_load_status=status,
    )
    monkeypatch.setattr(doctor_footprint, "_spawn_load_snapshot", lambda: snapshot)


def _pin_capacity(monkeypatch, cores: int):
    from fno import doctor_footprint

    monkeypatch.setattr(doctor_footprint, "_cpu_capacity_cores", lambda: cores)


def test_ac9_edge_ps_timeout_is_unavailable(monkeypatch) -> None:
    from fno import doctor_footprint

    def timed_out(argv, **kwargs):
        raise subprocess.TimeoutExpired(argv, kwargs["timeout"])

    monkeypatch.setattr(doctor_footprint.subprocess, "run", timed_out)

    output, error = doctor_footprint._read_ps(timeout=5.0)

    assert output is None
    assert error == "ps unavailable: timed out after 5.0s"


def test_ac9_edge_default_ps_timeout_refuses_with_exit_four(monkeypatch) -> None:
    from fno import doctor_footprint

    calls: list[float | None] = []

    def timed_out(argv, **kwargs):
        calls.append(kwargs.get("timeout"))
        raise subprocess.TimeoutExpired(argv, kwargs.get("timeout"))

    monkeypatch.setattr(doctor_footprint.subprocess, "run", timed_out)

    output, error = doctor_footprint._read_ps()

    assert calls == [doctor_footprint.PS_TIMEOUT_SECONDS]
    assert output is None
    assert error == "ps unavailable: timed out after 5.0s"


def test_ac9_edge_ps_timeout_caller_refuses_with_exit_four(monkeypatch) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint,
        "_read_ps",
        lambda **_kwargs: (None, "ps unavailable: timed out after 5.0s"),
    )

    result = runner.invoke(app, ["doctor", "footprint", "--json"])

    assert result.exit_code == 4, result.output
    assert json.loads(result.stdout) == {
        "error": "ps unavailable: timed out after 5.0s",
        "exit_code": 4,
    }


def test_live_root_pids_includes_live_detached_opencode_serve(monkeypatch, tmp_path) -> None:
    from fno import doctor_footprint

    (tmp_path / "opencode-serve.json").write_text(
        json.dumps({"pid": 900, "pid_start": 123}), encoding="utf-8"
    )
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path))
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map", lambda: {}
    )
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda pid, _start: True if pid == 900 else None,
    )

    assert doctor_footprint._live_root_pids() == (set(), None)
    assert doctor_footprint._live_shared_serve_root_pids() == ({900}, None)

    (tmp_path / "opencode-serve.json").write_text(
        json.dumps({"pid": 901, "pid_start": 123}), encoding="utf-8"
    )
    assert doctor_footprint._live_shared_serve_root_pids() == (
        set(),
        "shared serve root liveness unavailable",
    )

    (tmp_path / "opencode-serve.json").write_text(
        json.dumps({"pid": 900, "pid_start": None}), encoding="utf-8"
    )
    assert doctor_footprint._live_shared_serve_root_pids() == (
        set(),
        "shared serve root liveness unavailable",
    )


def test_live_root_pids_refuses_registry_pid_without_start_token(monkeypatch) -> None:
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=902,
        pid_start_time=None,
        harness="opencode",
        short_id="oc",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])

    assert doctor_footprint._live_root_pids() == (
        set(),
        "worker root liveness unavailable",
    )


def test_live_root_pids_refuses_unknown_registry_liveness(monkeypatch) -> None:
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=902,
        pid_start_time=123,
        harness="opencode",
        short_id="oc",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda _pid, _start: None,
    )

    assert doctor_footprint._live_root_pids() == (
        set(),
        "worker root liveness unavailable",
    )


def test_live_root_pids_refuses_registered_root_that_dies_after_snapshot(monkeypatch) -> None:
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=902,
        pid_start_time=123,
        harness="opencode",
        short_id="oc",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda _pid, _start: False,
    )

    assert doctor_footprint._live_root_pids(snapshot_pids={902}) == (
        set(),
        "worker root liveness unavailable",
    )


def test_live_root_pids_refuses_completed_root_that_matches_snapshot(monkeypatch) -> None:
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="exited",
        pid=902,
        pid_start_time=123,
        harness="opencode",
        short_id="oc",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda _pid, _start: False,
    )

    assert doctor_footprint._live_root_pids(snapshot_pids={902}) == (
        set(),
        "worker root liveness unavailable",
    )


def test_live_root_pids_refuses_terminal_root_cleared_after_snapshot(monkeypatch) -> None:
    from datetime import datetime, timezone
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="exited",
        pid=None,
        pid_start_time=None,
        last_reconciled_at=None,
        exited_at=datetime.now(timezone.utc).isoformat(),
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])

    assert doctor_footprint._live_root_pids(
        snapshot_pids={902}, snapshot_at=0.0
    ) == (
        set(),
        "worker root liveness unavailable",
    )


def test_live_root_pids_ignores_checked_stamp_on_terminal_root(monkeypatch) -> None:
    # A CHECKED bump (last_reconciled_at) inside the measurement window is not
    # an exit transition; reading it as one refused healthy measurements.
    from datetime import datetime, timezone
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="exited",
        pid=None,
        pid_start_time=None,
        last_reconciled_at=datetime.now(timezone.utc).isoformat(),
        exited_at=None,
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])

    assert doctor_footprint._live_root_pids(
        snapshot_pids={902}, snapshot_at=0.0
    ) == (set(), None)


def test_terminal_row_stamp_compares_at_stored_precision() -> None:
    # Transition stamps are whole-second; a stamp of T covers [T, T+1), so a
    # snapshot inside that second cannot rule out a later transition.
    from datetime import datetime, timedelta, timezone
    from fno import doctor_footprint
    from types import SimpleNamespace

    def row(exited_at):
        return SimpleNamespace(exited_at=exited_at)

    base = datetime(2026, 8, 25, 10, 0, 59, tzinfo=timezone.utc)
    changed = doctor_footprint._terminal_row_changed_after_snapshot
    assert changed(row(base.strftime("%Y-%m-%dT%H:%M:%SZ")), base.timestamp() + 0.5)
    assert not changed(
        row((base - timedelta(seconds=2)).strftime("%Y-%m-%dT%H:%M:%SZ")),
        base.timestamp() + 0.5,
    )
    assert changed(
        row(base.replace(microsecond=700000).isoformat()), base.timestamp() + 0.5
    )
    assert not changed(
        row(base.replace(microsecond=300000).isoformat()), base.timestamp() + 0.5
    )
    assert not changed(row(None), base.timestamp() + 0.5)
    assert changed(row("not-a-stamp"), base.timestamp() + 0.5)


def test_live_root_pids_refuses_incomplete_registry(monkeypatch) -> None:
    from fno import doctor_footprint
    from fno.agents.registry import LoadedRegistry

    monkeypatch.setattr(
        "fno.agents.registry.load_registry",
        lambda: LoadedRegistry([], complete=False),
    )

    assert doctor_footprint._live_root_pids() == (
        set(),
        "worker registry incomplete",
    )


def test_live_root_pids_includes_roster_resolved_claude_bg_worker(monkeypatch) -> None:
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        harness="claude",
        short_id="cl-bg",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map",
        lambda **_kwargs: {"cl-bg": 902},
    )
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda pid, _start: pid == 902,
    )

    assert doctor_footprint._live_root_pids() == ({902}, None)


def test_shared_serve_root_refuses_root_that_dies_after_snapshot(monkeypatch, tmp_path) -> None:
    from fno import doctor_footprint

    (tmp_path / "opencode-serve.json").write_text(
        json.dumps({"pid": 900, "pid_start": 123}), encoding="utf-8"
    )
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path))
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda _pid, _start: False,
    )

    assert doctor_footprint._live_shared_serve_root_pids(snapshot_pids={900}) == (
        set(),
        "shared serve root liveness unavailable",
    )


def test_live_root_pids_refuses_unavailable_pidless_worker_discovery(monkeypatch) -> None:
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        harness="claude",
        short_id="cl-bg",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map",
        lambda **_kwargs: {},
    )
    # x-a457: the map missing the row is no longer proof of death on its own;
    # an UNREADABLE roster oracle is what keeps this row in the gap.
    monkeypatch.setattr("fno.agents.session_procs.roster_pid_map", lambda: None)

    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)
    assert "socket map" in error.text


def test_live_root_pids_drops_routed_corpse_row_dead_in_both_daemon_oracles(
    monkeypatch,
) -> None:
    """x-a457: a routed row in NEITHER the rv socket farm nor the claude roster
    names a session the daemon no longer holds. It is a corpse the registry
    never retired, not an unattributed live process, so the reading stops
    calling its (nonexistent) cost a gap."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="claude",
        short_id="deadbee",
        name="corpse",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map",
        lambda **_kwargs: {},
    )
    monkeypatch.setattr("fno.agents.session_procs.roster_pid_map", lambda: {})

    assert doctor_footprint._live_root_pids() == (set(), None)


def test_live_root_pids_attributes_routed_row_through_roster_pid(monkeypatch) -> None:
    """The roster is the second oracle AND a fallback attribution: its pid is
    the PTY host hosting the session, a real process the reading should
    attribute when the rv socket farm missed it."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="claude",
        short_id="alive123",
        name="hosted",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map",
        lambda **_kwargs: {},
    )
    monkeypatch.setattr(
        "fno.agents.session_procs.roster_pid_map", lambda: {"alive123": 903}
    )
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda pid, _start: pid == 903,
    )

    assert doctor_footprint._live_root_pids() == ({903}, None)


def test_live_root_pids_joins_a_full_uuid_short_id_through_the_derived_key(
    monkeypatch,
) -> None:
    """register_existing_session writes the FULL session uuid into a claude
    row's short_id when no transport key was given at birth (the SessionStart
    hook passes none). The daemon maps key on the DERIVED 8-hex, so the join
    must derive, or a live hook-registered session reads as a corpse."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="idle",
        pid=None,
        pid_start_time=None,
        harness="claude",
        short_id="e6f78b98-e594-47ed-ad81-84f8a78b8bb7",
        harness_session_id="e6f78b98-e594-47ed-ad81-84f8a78b8bb7",
        name="e6f78b98",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map",
        lambda **_kwargs: {},
    )
    monkeypatch.setattr(
        "fno.agents.session_procs.roster_pid_map", lambda: {"e6f78b98": 903}
    )
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda pid, _start: pid == 903,
    )

    assert doctor_footprint._live_root_pids() == ({903}, None)


def test_resolve_session_pid_derives_the_join_key_from_a_full_uuid_short_id() -> None:
    """The cost view joins the same rv map as the gate: a hook-registered row
    whose short_id holds the full uuid must derive the 8-hex key, or the
    session resolves to its recorded (absent) pid and its cost vanishes."""
    from fno.agents.session_procs import resolve_session_pid

    assert (
        resolve_session_pid(
            harness="claude",
            short_id="e6f78b98-e594-47ed-ad81-84f8a78b8bb7",
            session_id="e6f78b98-e594-47ed-ad81-84f8a78b8bb7",
            socket_map={"e6f78b98": 903},
        )
        == 903
    )


def test_live_root_pids_routes_a_claude_row_with_an_empty_short_id(
    monkeypatch,
) -> None:
    """Rows minted before the birth fix carry an empty short_id but a real
    session id: the claude daemon maps can still answer for them, so the row
    routes and its derived key joins - it must not sit unrouted forever."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="claude",
        short_id="",
        harness_session_id="2529b52b-2477-4c1e-9d3a-1a2b3c4d5e6f",
        name="king-119e-reap-branch-2529b52b",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map",
        lambda **_kwargs: {},
    )
    monkeypatch.setattr(
        "fno.agents.session_procs.roster_pid_map", lambda: {"2529b52b": 903}
    )
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda pid, _start: pid == 903,
    )

    assert doctor_footprint._live_root_pids() == ({903}, None)


def test_live_root_pids_keeps_a_roster_held_row_whose_pid_entry_is_not_usable(
    monkeypatch,
) -> None:
    """The daemon writes a roster worker's pid optional and has drifted field
    types before. A session the roster HOLDS with no usable pid exists: that
    is no answer, not a death certificate."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="claude",
        short_id="deadbee",
        name="hosted",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map",
        lambda **_kwargs: {},
    )
    monkeypatch.setattr(
        "fno.agents.session_procs.roster_pid_map", lambda: {"deadbee": None}
    )

    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)


def test_live_root_pids_suppresses_on_a_roster_held_row_with_a_dead_pid(
    monkeypatch,
) -> None:
    """A daemon-held record whose pid died is the same fact as a dead
    socket-map pid: the socket arm suppresses the report for it, so the
    roster arm must not answer "corpse" instead - the keeper may be mid
    re-adoption."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="claude",
        short_id="deadbee",
        name="hosted",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.bg_socket_pid_map",
        lambda **_kwargs: {},
    )
    monkeypatch.setattr(
        "fno.agents.session_procs.roster_pid_map", lambda: {"deadbee": 404}
    )
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda pid, _start: False,
    )

    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert error == "worker root liveness unavailable"


def test_live_root_pids_drops_unrouted_row_with_expired_claim(monkeypatch) -> None:
    """x-a457: an unrouted row whose worker claim store positively reports no
    live holder is a corpse row. The 14 rows that kept this box's spawn gate
    refusing were exactly this population - claims expired, rows still live."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="codex",
        short_id="",
        name="t-stale-lane",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(doctor_footprint, "_claim_witness", lambda _name: "stale")

    assert doctor_footprint._live_root_pids() == (set(), None)


def test_claim_witness_answers_nothing_for_a_row_that_never_claimed(
    tmp_path, monkeypatch
) -> None:
    """claim_status reports "free" for a claim file that never existed, and a
    lane that never claimed (an operator-registered session) is not dead on
    that account: no file means the store has no answer, not a death
    certificate."""
    from fno import doctor_footprint

    monkeypatch.setattr(
        "fno.agents.spawn_gate._gate_claims_root", lambda: tmp_path
    )

    assert doctor_footprint._claim_witness("never-registered") is None


def test_live_root_pids_keeps_unrouted_row_whose_claim_is_live(monkeypatch) -> None:
    """The fail-closed half: a live codex thread lane holds a live claim and
    its cost sits in the unattributed app-server, so it stays a NAMED gap."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="codex",
        short_id="",
        name="t-live-lane",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(doctor_footprint, "_claim_witness", lambda _name: "live")

    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)
    assert "codex" in error.text


def test_live_root_pids_keeps_unrouted_row_on_unreadable_claim_store(monkeypatch) -> None:
    """An unreadable claim store proves nothing: fail closed, gap row."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="codex",
        short_id="",
        name="t-unreadable",
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(doctor_footprint, "_claim_witness", lambda _name: None)

    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)


def test_live_root_pids_pane_row_costs_the_attributed_mux_server(monkeypatch) -> None:
    """A pane burns CPU inside the mux server process the reading attributes;
    whatever the probe answers, the pane adds no unattributed cost. Only an
    answer the mux could NOT give leaves the cost unproven."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="codex",
        short_id="",
        name="t-pane",
        mux={"session": "main", "pane_id": 7},
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.mux_spawn._mux_pane_alive", lambda _mux, **_kwargs: True
    )

    assert doctor_footprint._live_root_pids() == (set(), None)


def test_live_root_pids_refuses_pidless_live_pane(monkeypatch) -> None:
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="codex",
        short_id="",
        mux={"session": "main", "pane_id": 7},
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    # x-a457: the pane's cost rides the attributed mux server, so only a probe
    # the mux could not ANSWER (None) leaves the row a gap row.
    monkeypatch.setattr(
        "fno.agents.mux_spawn._mux_pane_alive", lambda _mux, **_kwargs: None
    )

    # x-e040: one pidless non-claude row is a NAMED attribution gap, not a
    # dead reading. The old contract killed the whole report here.
    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)
    assert "codex" in error.text


def test_ac9_edge_cause_payload_is_bounded(monkeypatch) -> None:
    from fno import doctor_footprint
    from fno.footprint import parse_footprint

    rows = "\n".join(
        f"{100 + i} 1 01:00:00 {100 - i}.0 1024 fno-agents-worker worker-{i} "
        + "x" * 5000
        for i in range(10)
    )
    reading = parse_footprint(f"PID PPID ELAPSED %CPU RSS COMMAND\n{rows}")

    payload = doctor_footprint._payload(
        reading,
        process_threshold=None,
        exit_code=0,
        top_limit=5,
        command_limit=64,
    )

    assert len(payload["top"]) == 5
    assert all(
        len(json.dumps(item["command"]).encode("utf-8")) <= 64
        for item in payload["top"]
    )

    reading.top[0] = (100.0, "😀" * 1000)
    payload = doctor_footprint._payload(
        reading,
        process_threshold=None,
        exit_code=0,
        top_limit=1,
        command_limit=64,
    )
    assert len(json.dumps(payload["top"][0]["command"]).encode("utf-8")) <= 64


def test_ac9_edge_cause_text_output_is_bounded(capsys) -> None:
    # The JSON payload bounds cause commands; the text branch must too, or a
    # --cause-only run floods the terminal with full ps command lines.
    import typer

    from fno import doctor_footprint
    from fno.footprint import parse_footprint

    rows = "\n".join(
        f"{100 + i} 1 01:00:00 {100 - i}.0 1024 fno-agents-worker worker-{i} "
        + "x" * 5000
        for i in range(10)
    )
    reading = parse_footprint(f"PID PPID ELAPSED %CPU RSS COMMAND\n{rows}")

    with pytest.raises(typer.Exit):
        doctor_footprint._emit_result(
            reading, process_threshold=None, json_output=False, cause_only=True
        )

    lines = capsys.readouterr().out.splitlines()
    consumers = [ln for ln in lines if "fno-agents-worker" in ln]
    assert len(consumers) == 5
    assert all(len(ln) <= 1200 for ln in consumers)
    assert all("... (" in ln for ln in consumers)


def test_ac1_hp_live_rows_read_the_registry_in_process(monkeypatch) -> None:
    # AC1-HP: the count comes from load_registry, not from a subprocess whose
    # 8.5s-to-21.7s answer never fit the 5.0s budget it was given.
    from types import SimpleNamespace

    from fno import doctor_footprint

    rows = [
        SimpleNamespace(status="live"),
        SimpleNamespace(status="busy"),
        SimpleNamespace(status="exited"),
    ]
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)

    def no_subprocess(*_args, **_kwargs):
        raise AssertionError("the roster count must not shell out")

    monkeypatch.setattr(doctor_footprint.subprocess, "run", no_subprocess)

    rows, error = doctor_footprint.live_registry_rows()

    assert error is None
    assert len(rows) == 2


def test_ac1_edge_incomplete_registry_names_the_registry_not_a_timeout(
    monkeypatch,
) -> None:
    # AC1-EDGE: the degraded note must name the registry. A "timed out" reason
    # would describe a subprocess this path no longer runs.
    from types import SimpleNamespace

    from fno import doctor_footprint

    class _Incomplete(list):
        complete = False

    rows = _Incomplete([SimpleNamespace(status="live")])
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)

    rows, error = doctor_footprint.live_registry_rows()

    assert rows is None
    assert error == "roster unavailable: worker registry incomplete"
    assert "timed out" not in error


def test_ac1_edge_unreadable_registry_degrades_with_a_named_reason(
    monkeypatch,
) -> None:
    from fno import doctor_footprint

    def boom():
        raise OSError("graph.json is a directory")

    monkeypatch.setattr("fno.agents.registry.load_registry", boom)

    rows, error = doctor_footprint.live_registry_rows()

    assert rows is None
    assert error is not None and "registry unreadable" in error


def test_ac9_edge_ps_output_with_invalid_utf8_degrades_not_crashes(monkeypatch) -> None:
    # A process's argv may legally carry non-UTF-8 bytes; one such byte in the
    # snapshot must degrade that command string, not kill the verb.
    from types import SimpleNamespace

    from fno import doctor_footprint

    def raw_bytes_ps(argv, **kwargs):
        with open(kwargs["stdout"].name, "wb") as raw:
            raw.write(b"PID PPID ELAPSED %CPU RSS COMMAND\n"
                      b"100 1 01:00:00 86.0 1024 fno-agents-worker --run \xff\xfe\n")
        return SimpleNamespace(returncode=0, stderr="")

    monkeypatch.setattr(doctor_footprint.subprocess, "run", raw_bytes_ps)

    output, error = doctor_footprint._read_ps()

    assert error is None
    assert "\N{REPLACEMENT CHARACTER}" in output


def test_broken_spawn_gate_import_is_loud_not_misdiagnosed(monkeypatch) -> None:
    # A renamed spawn_gate private must surface as ImportError, not degrade
    # every reading to "worker root discovery unavailable".
    from types import SimpleNamespace

    from fno import doctor_footprint
    from fno.agents import spawn_gate as gate_module

    row = SimpleNamespace(
        status="live", pid=5, pid_start_time=1, harness="codex", short_id="cd"
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.delattr(gate_module, "_pid_alive")
    monkeypatch.delattr(gate_module, "_process_start_time")

    with pytest.raises(ImportError):
        doctor_footprint._live_root_pids(snapshot_pids=set())


def test_ac5_edge_capacity_uses_affinity_on_python_without_process_cpu_count(
    monkeypatch,
) -> None:
    from fno import doctor_footprint

    monkeypatch.delattr(doctor_footprint.os, "process_cpu_count", raising=False)
    monkeypatch.setattr(doctor_footprint.os, "cpu_count", lambda: 64)
    monkeypatch.setattr(
        doctor_footprint.os,
        "sched_getaffinity",
        lambda _pid: {0, 1},
        raising=False,
    )

    assert doctor_footprint._cpu_capacity_cores() == 2


def test_ac5_edge_capacity_honors_cpu_quota(monkeypatch) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(doctor_footprint, "_cpu_quota_cores", lambda: 2.0)
    monkeypatch.setattr(
        doctor_footprint.os,
        "process_cpu_count",
        lambda: 64,
        raising=False,
    )
    monkeypatch.setattr(doctor_footprint.os, "cpu_count", lambda: 64)
    monkeypatch.setattr(
        doctor_footprint.os,
        "sched_getaffinity",
        lambda _pid: set(range(64)),
        raising=False,
    )

    assert doctor_footprint._cpu_capacity_cores() == 2


def test_ac5_hp_json_reports_fleet_totals_and_cpu_shares(
    monkeypatch, no_worker_roots
) -> None:
    from fno import doctor_footprint

    _pin_load(monkeypatch, status="within")
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            """\
            PID PPID ELAPSED %CPU RSS COMMAND
            100 1 01:00:00 20.0 1024 fno-agents-worker --run
            101 100 00:00:05 80.0 1024 cargo test -p fno
            200 1 01:00:00 100.0 1024 unrelated-build
            """,
            [{"name": "worker-a"}],
            [],
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    capacity = doctor_footprint._cpu_capacity_cores()
    assert payload["descendant_cpu_cores"] == pytest.approx(0.8)
    assert payload["fleet_cpu_cores"] == pytest.approx(1.0)
    assert payload["descendant_process_count"] == 1
    assert payload["cpu_capacity_cores"] == capacity
    assert payload["fleet_percent_capacity"] == pytest.approx(100 / capacity)
    assert payload["fleet_percent_measured_cpu"] == pytest.approx(50.0)


def test_spawn_load_snapshot_is_rendered_in_text_and_json(
    monkeypatch, capsys
) -> None:
    from types import SimpleNamespace

    from fno import doctor_footprint
    from fno.agents import spawn_gate

    reading = parse_footprint(
        "PID PPID ELAPSED %CPU RSS COMMAND\n"
        "100 1 01:00:00 49.0 1024 fno-agents-worker --run\n"
    )
    settings = SimpleNamespace(
        agents=SimpleNamespace(max_load_per_cpu=8.0),
    )
    snapshot = SimpleNamespace(
        load_1m=141.6,
        max_load_per_cpu=8.0,
        load_ceiling=96.0,
        load_cpu_count=12,
        spawn_load_status="exceeded",
    )
    monkeypatch.setattr("fno.config.load_settings", lambda: settings)
    monkeypatch.setattr(spawn_gate, "_load_snapshot", lambda _factor: snapshot)
    monkeypatch.setattr(doctor_footprint, "_cpu_capacity_cores", lambda: 12)

    payload = doctor_footprint._payload(
        reading, process_threshold=None, exit_code=0
    )

    assert payload["load_1m"] == pytest.approx(141.6)
    assert payload["max_load_per_cpu"] == pytest.approx(8.0)
    assert payload["load_ceiling"] == pytest.approx(96.0)
    assert payload["load_cpu_count"] == 12
    assert payload["spawn_load_status"] == "exceeded"

    with pytest.raises(typer.Exit):
        doctor_footprint._emit_result(
            reading, process_threshold=None, json_output=False
        )
    assert "spawn load: 141.6 against 96.0" in capsys.readouterr().out


def test_ac6_edge_cause_only_excludes_observer_subtree_and_skips_roster(
    monkeypatch, no_worker_roots
) -> None:
    from fno import doctor_footprint

    observer_pid = os.getpid()
    calls: list[list[str]] = []
    monkeypatch.setattr(
        doctor_footprint,
        "_live_root_pids",
        lambda **_kwargs: (set(), None),
    )
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            f"""\
            PID PPID ELAPSED %CPU RSS COMMAND
            {observer_pid} 1 01:00:00 20.0 1024 fno-py doctor footprint
            999 {observer_pid} 01:00:00 80.0 1024 ps -Ao pid,ppid
            100 1 01:00:00 20.0 1024 fno-agents-worker --run
            101 100 01:00:00 80.0 1024 cargo test -p fno
            """,
            [],
            calls,
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint", "--json", "--cause-only"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["process_count"] == 2
    assert payload["fleet_cpu_cores"] == pytest.approx(1.0)
    # Git calls the config-root resolver may shell are not the cause-only
    # contract's subject; what it promises is ONE ps read and no roster walk.
    assert [call for call in calls if call[0] == "ps"] == [
        ["ps", "-Ao", "pid,ppid,etime,%cpu,rss,command"]
    ]
    assert not [call for call in calls if "agents" in call]


def test_ac6_edge_cause_only_seeds_live_detached_registry_root(
    monkeypatch, no_worker_roots
) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint,
        "_live_root_pids",
        lambda **_kwargs: ({100}, None),
    )
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            """\
            PID PPID ELAPSED %CPU RSS COMMAND
            100 1 01:00:00 20.0 1024 opencode serve --detach
            101 100 01:00:00 80.0 1024 cargo test -p fno
            200 1 01:00:00 90.0 1024 cargo test -p unrelated
            """,
            [],
            [],
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint", "--json", "--cause-only"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["descendant_process_count"] == 1
    assert payload["fleet_cpu_cores"] == pytest.approx(1.0)


def test_ac6_edge_cause_only_refuses_root_missing_from_snapshot(
    monkeypatch, no_worker_roots
) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint,
        "_live_root_pids",
        lambda **_kwargs: ({999}, None),
    )
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            """\
            PID PPID ELAPSED %CPU RSS COMMAND
            100 1 01:00:00 20.0 1024 fno-agents-worker --run
            """,
            [],
            [],
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint", "--json", "--cause-only"])

    assert result.exit_code == 4
    assert "missing from ps snapshot" in result.stdout


def test_sustained_cpu_threshold_derives_from_capacity_and_honors_override(
    monkeypatch,
) -> None:
    """The old absolute 1.0 asked a 12-core machine's fleet to idle at 8%.
    The threshold is now a fraction of capacity; a config override pins it
    absolutely for a small box."""
    from fno import doctor_footprint as df

    assert df.sustained_cpu_threshold(12) == pytest.approx(1.2)
    assert df.sustained_cpu_threshold(1) == pytest.approx(
        df.SUSTAINED_CPU_FLOOR_CORES
    )
    monkeypatch.setattr(df, "_footprint_cpu_override", lambda: 2.0)
    assert df.sustained_cpu_threshold(12) == pytest.approx(2.0)


def test_ac7_edge_short_lived_descendant_counts_in_fleet_cpu(
    monkeypatch, no_worker_roots
) -> None:
    """A 100% descendant for 5s lands in the fleet's CPU reading. It no longer
    decides the exit on its own: sustained CPU is reported against a derived
    threshold, while the exit answers the two alarms (capacity, leak)."""
    from fno import doctor_footprint

    _pin_load(monkeypatch, status="within")
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            """\
            PID PPID ELAPSED %CPU RSS COMMAND
            100 1 01:00:00 20.0 1024 fno-agents-worker --run
            101 100 00:00:05 100.0 1024 cargo test -p fno
            """,
            [{"name": "worker-a"}],
            [],
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 0, result.output
    assert "fleet CPU: 1.200 cores" in result.stdout
    assert "verdict: within on load_1m" in result.stdout


def test_ac8_edge_descendants_do_not_consume_direct_process_threshold(
    monkeypatch, no_worker_roots
) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            """\
            PID PPID ELAPSED %CPU RSS COMMAND
            100 1 01:00:00 20.0 1024 fno-agents-worker --run
            101 100 01:00:00 20.0 1024 cargo test -p fno
            102 101 01:00:00 20.0 1024 rustc --crate-name fno
            """,
            [{"name": "worker-a"}],
            [],
        ),
    )

    _pin_load(monkeypatch, status="within")

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 0, result.output
    assert "processes: 3" in result.stdout
    assert "unexplained processes: 0 (1 direct, roster explains 2)" in result.stdout


def test_ac9_edge_cpu_share_uses_constrained_capacity(
    monkeypatch, no_worker_roots
) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(doctor_footprint.os, "cpu_count", lambda: 64)
    monkeypatch.setattr(doctor_footprint.os, "process_cpu_count", lambda: 2, raising=False)
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            """\
            PID PPID ELAPSED %CPU RSS COMMAND
            100 1 01:00:00 100.0 1024 fno-agents-worker --run
            """,
            [],
            [],
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint", "--json", "--cause-only"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["cpu_capacity_cores"] == 2
    assert payload["fleet_percent_capacity"] == pytest.approx(50.0)


def test_ac3_hp_reports_both_thresholds_and_exits_zero(
    monkeypatch, no_worker_roots
) -> None:
    from fno import doctor_footprint

    calls: list[list[str]] = []
    _pin_load(monkeypatch, status="within")
    _pin_capacity(monkeypatch, 4)
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            """\
            PID ELAPSED %CPU RSS COMMAND
            101 01:00:00 20.0 1024 fno mux serve
            102 00:00:01 92.0 1024 fno --version
            """,
            [{"name": "worker-a"}, {"name": "worker-b"}],
            calls,
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 0, result.output
    assert "sustained CPU: 0.200 cores (threshold 0.400 from 4 cpus; a separate axis - it did not decide the verdict)" in result.stdout
    assert "processes: 2" in result.stdout
    assert "unexplained processes: 0 (2 direct, roster explains 3)" in result.stdout
    assert "transient calls: 1" in result.stdout
    # ps is the only subprocess left: the roster count reads the registry
    # in process, so there is no second shell-out to budget.
    assert [call for call in calls] == [
        ["ps", "-Ao", "pid,ppid,etime,%cpu,rss,command"],
    ]


def test_ac4_edge_capacity_over_exits_three_and_names_top_consumers(
    monkeypatch, no_worker_roots
) -> None:
    """Capacity and leak BOTH fire; the capacity exit (3) wins as the more
    urgent alarm and the leak still prints with its own words."""
    from fno import doctor_footprint

    _pin_load(monkeypatch, status="exceeded")
    _pin_capacity(monkeypatch, 4)
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            """\
            PID ELAPSED %CPU RSS COMMAND
            201 02:00:00 80.0 1024 fno mux serve
            202 01:00:00 40.0 2048 fno-agents-daemon --serve
            """,
            [],
            [],
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 3
    assert "verdict: capacity over on load_1m" in result.stdout
    assert "unexplained processes: 1 (2 direct, roster explains 1)" in result.stdout
    assert "fno mux serve (80.0%)" in result.stdout
    assert "fno-agents-daemon --serve (40.0%)" in result.stdout


def test_ac4_edge_unexplained_processes_get_their_own_exit(
    monkeypatch, no_worker_roots
) -> None:
    """A leak without a capacity breach exits 5 - the leak's own code, not the
    capacity code the old merged verdict borrowed (defect 1 in the plan)."""
    from fno import doctor_footprint

    _pin_load(monkeypatch, status="within")
    _pin_capacity(monkeypatch, 4)
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            """\
            PID ELAPSED %CPU RSS COMMAND
            211 02:00:00 10.0 1024 fno worker-a
            212 02:00:00 10.0 1024 fno worker-b
            """,
            [],
            [],
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 5
    assert "verdict: leak on load_1m" in result.stdout
    assert "sustained CPU: 0.200 cores" in result.stdout
    assert "processes: 2" in result.stdout
    assert "unexplained processes: 1 (2 direct, roster explains 1)" in result.stdout


def test_ac5_edge_roster_failure_degrades_the_threshold_not_the_reading(
    monkeypatch, no_worker_roots
) -> None:
    """x-e040: the roster is an enrichment. On roster failure the measurement
    still prints, with the threshold degraded away and the reason named. The
    old contract killed the whole report (exit 4, no reading)."""
    from fno import doctor_footprint

    calls: list[list[str]] = []

    def ps_only(argv, **kwargs):
        calls.append(list(argv))
        if argv[0] == "ps":
            kwargs["stdout"].write(
                "PID ELAPSED %CPU RSS COMMAND\n101 01:00:00 20.0 1024 fno daemon\n"
            )
            return subprocess.CompletedProcess(argv, 0)
        raise AssertionError(f"unexpected subprocess in a footprint run: {argv}")

    def unreadable_registry():
        raise OSError("registry is a directory")

    monkeypatch.setattr(doctor_footprint.subprocess, "run", ps_only)
    monkeypatch.setattr("fno.agents.registry.load_registry", unreadable_registry)
    _pin_load(monkeypatch, status="within")
    _pin_capacity(monkeypatch, 4)

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 0
    assert "roster unavailable" in result.stdout
    assert "unexplained processes: unknown" in result.stdout
    assert "processes:" in result.stdout
    assert "degraded: roster unavailable" in result.stdout
    # The roster no longer costs a subprocess: ps is the only one left.
    assert [call[0] for call in calls] == ["ps"]


def test_ac7_edge_json_contains_thresholds_and_exit_meaning(
    monkeypatch, no_worker_roots
) -> None:
    from fno import doctor_footprint

    _pin_load(monkeypatch, status="within")
    _pin_capacity(monkeypatch, 10)
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            """\
            PID ELAPSED %CPU RSS COMMAND
            301 00:00:01 92.0 1024 fno --version
            """,
            [{"name": "worker-a"}],
            [],
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["sustained_cpu_cores"] == 0.0
    assert payload["transient_call_count"] == 1
    # The threshold derives from capacity (0.1 x 10), not the old constant.
    assert payload["sustained_cpu_threshold_cores"] == pytest.approx(1.0)
    assert payload["direct_process_count_threshold"] == 2
    assert payload["leak_verdict"] == "clean"
    assert payload["capacity_verdict"] == "within"
    assert payload["exit_code"] == 0


def _pidless_row(harness: str, *, name: str = "w1", node: str | None = None):
    from types import SimpleNamespace

    return SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness=harness,
        short_id=None,
        name=name,
        node=node,
    )


def test_pidless_nonclaude_row_is_a_named_gap_not_a_dead_reading(monkeypatch):
    """x-e040, the falsifiable discriminator: a pidless live CODEX row is
    present, and the reading still ANSWERS with the gap named."""
    from fno import doctor_footprint

    monkeypatch.setattr(
        "fno.agents.registry.load_registry", lambda: [_pidless_row("codex")]
    )
    monkeypatch.setattr(doctor_footprint, "_claim_witness", lambda _name: "live")
    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)
    assert "codex" in error.text
    assert "w1 (node=unknown)" in error.text


def test_pidless_unknown_harness_row_is_the_same_named_gap(monkeypatch):
    """The anti-hardcode assertion: a harness this code has never heard of
    degrades exactly like codex. No name list, no crash, no dead reading."""
    from fno import doctor_footprint

    monkeypatch.setattr(
        "fno.agents.registry.load_registry", lambda: [_pidless_row("luna")]
    )
    monkeypatch.setattr(doctor_footprint, "_claim_witness", lambda _name: "live")
    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)
    assert "luna" in error.text


def test_no_pidless_rows_still_yields_a_clean_reading(monkeypatch):
    """The other half of the discriminator: without the pidless row there is
    no gap, so a green run cannot hide behind an always-gap."""
    from fno import doctor_footprint

    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
    assert doctor_footprint._live_root_pids() == (set(), None)


def test_gap_reading_still_prints_the_measurement_and_exits_four(monkeypatch):
    """The acceptance: the verb prints a CPU and process-count reading WITH a
    named degradation. Exit 4 stays (gating unavailable), but the measurement
    is present in exactly the condition that used to print only an error."""
    from fno import doctor_footprint

    reading = doctor_footprint.parse_footprint(
        "PID PPID ELAPSED %CPU RSS COMMAND\n100 1 01:00:00 0.5 1024 fno daemon\n",
        excluded_root_pids=set(),
        attributed_root_pids=set(),
        threshold_excluded_root_pids=set(),
    )._replace(attribution_gap="1 pidless codex row(s) unresolved")
    monkeypatch.setattr(
        doctor_footprint, "cause_reading", lambda: (reading, None)
    )
    _pin_load(monkeypatch, status="within")
    result = runner.invoke(app, ["doctor", "footprint", "--json", "--cause-only"])
    assert result.exit_code == 4, result.output
    payload = json.loads(result.stdout)
    assert payload["process_count"] >= 1
    assert "fleet_cpu_cores" in payload
    assert "codex" in payload["attribution_gap"]
    assert payload["exit_code"] == 4


def test_cause_only_reports_a_real_capacity_verdict(monkeypatch):
    """x-a457's done probe: a clean cause-only reading answers the capacity
    question (within/near/over) instead of a structural unknown, and carries
    no attribution_gap key. Exit codes do not move: 0 clean, 4 gapped - the
    Rust gate reads stdout only on exit 0."""
    from fno import doctor_footprint

    reading = doctor_footprint.parse_footprint(
        "PID PPID ELAPSED %CPU RSS COMMAND\n100 1 01:00:00 0.5 1024 fno daemon\n",
        excluded_root_pids=set(),
        attributed_root_pids=set(),
        threshold_excluded_root_pids=set(),
    )
    monkeypatch.setattr(
        doctor_footprint, "cause_reading", lambda: (reading, None)
    )
    _pin_load(monkeypatch, status="within")
    result = runner.invoke(app, ["doctor", "footprint", "--json", "--cause-only"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["capacity_verdict"] == "within"
    assert "attribution_gap" not in payload
    assert payload["exit_code"] == 0


def test_spawn_gate_treats_a_gap_reading_as_not_headroom(monkeypatch):
    """A gapped fleet share is an undercount; None is the gate's existing
    never-headroom answer, so the gate refuses above the trigger with the gap
    named instead of admitting on an undercount."""
    from fno import doctor_footprint
    from fno.agents import spawn_gate

    reading = doctor_footprint.parse_footprint(
        "PID PPID ELAPSED %CPU RSS COMMAND\n100 1 01:00:00 0.5 1024 fno daemon\n",
        excluded_root_pids=set(),
        attributed_root_pids=set(),
        threshold_excluded_root_pids=set(),
    )._replace(attribution_gap="1 pidless codex row(s) unresolved")
    monkeypatch.setattr(
        "fno.doctor_footprint.cause_reading", lambda: (reading, None)
    )
    assert spawn_gate._fleet_cpu_reading() is None
    assert spawn_gate._footprint_cause_evidence() is None


def test_capacity_verdict_names_its_axis_and_deciding_numbers(monkeypatch):
    """AC7-HP (x-5283): load over its ceiling while sustained CPU is under
    its threshold - the verdict names load_1m as its axis and prints the
    numbers that decided it, and the sustained line disclaims the verdict.
    On main neither surface said which axis produced the verdict, so the
    footprint's headroom reading and the gate's saturation refusal looked
    like a contradiction."""
    from fno import doctor_footprint

    reading = doctor_footprint.parse_footprint(
        "PID PPID ELAPSED %CPU RSS COMMAND\n100 1 01:00:00 0.5 1024 fno daemon\n",
        excluded_root_pids=set(),
        attributed_root_pids=set(),
        threshold_excluded_root_pids=set(),
    )
    monkeypatch.setattr(
        doctor_footprint, "cause_reading", lambda: (reading, None)
    )
    _pin_load(monkeypatch, status="exceeded", load=110.4, ceiling=96.0)
    result = runner.invoke(app, ["doctor", "footprint", "--json", "--cause-only"])
    payload = json.loads(result.stdout)
    assert payload["capacity_verdict"] == "over"
    assert payload["capacity_verdict_axis"] == "load_1m"
    assert payload["load_1m"] == 110.4
    assert payload["load_ceiling"] == 96.0

    shown = runner.invoke(app, ["doctor", "footprint", "--cause-only"])
    assert "verdict: over on load_1m (110.4 against 96.0)" in shown.output
    assert "a separate axis - it did not decide the verdict" in shown.output


# ---------------------------------------------------------------------------
# Orphaned cargo test binaries: ppid 1 + a deps/ binary argv[0], confirmed by
# a CACHEDIR.TAG in the owning target dir, and the --reap-orphans lever.
# ---------------------------------------------------------------------------

_CACHEDIR_TAG = "Signature: 8a477f597d28d172789f06886806bc55\n"


def _pid_alive(pid: int) -> bool:
    """True while the pid exists AND runs. A SIGKILLed child of this test
    process stays a zombie until its parent reaps it, and kill(pid, 0)
    answers a zombie happily, so the state letter decides."""
    out = subprocess.run(
        ["ps", "-o", "state=", "-p", str(pid)], capture_output=True, text=True
    ).stdout.strip()
    return bool(out) and not out.startswith("Z")


@pytest.fixture
def planted_orphan(tmp_path):
    """A real sleeper holding 25 unreaped zombie children, under a target dir
    with a CACHEDIR.TAG. Yields the pid; kills it at teardown.

    The probe's argv[0] is the deps-binary path shape via execv (no copy of a
    signed binary: macOS kills that at exec). Its 25 dead children make the
    zombie clause name it at any ppid - a CI runner is a child subreaper, so
    a backgrounded orphan there never reaches ppid 1, and the plant must not
    depend on the parentless clause."""
    import signal
    import time

    target = tmp_path / "crates" / "x" / "target"
    deps = target / "debug" / "deps"
    deps.mkdir(parents=True)
    (target / "CACHEDIR.TAG").write_text(_CACHEDIR_TAG)
    probe = deps / "probe-0123456789abcdef"
    inner = tmp_path / "probe_inner.py"
    inner.write_text(
        "import os, subprocess, time\n"
        "kids = [subprocess.Popen(['/bin/sleep', '0.05']) for _ in range(25)]\n"
        "time.sleep(1)\n"
        f"os.execv('/bin/sleep', [{str(probe)!r}, '300'])\n"
    )
    # One process throughout: sh execs python, which execv's into the
    # sleeper, so `holder.pid` stays the pid ps will report.
    holder = subprocess.Popen(
        ["/bin/sh", "-c", f"exec {sys.executable} {inner}"], stdout=subprocess.DEVNULL
    )
    pid = holder.pid
    # Wait until the execv lands and ps sees the probe path as argv[0].
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        out = subprocess.run(
            ["ps", "-o", "command=", "-p", str(pid)], capture_output=True, text=True
        ).stdout
        if "probe-0123456789abcdef" in out:
            break
        time.sleep(0.1)
    else:
        holder.kill()
        raise AssertionError("planted probe never reached its argv[0] shape")
    yield pid
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    holder.wait()  # reap: a killed child of this process must not linger


def test_orphan_confirmation_demands_a_cachedir_tag(tmp_path):
    """Path shape is a NAME match; CACHEDIR.TAG is the cargo-target proof. The
    source-tree shape (cli/src/fno/target) has no tag and is never named."""
    from fno import doctor_footprint
    from fno.footprint import parse_footprint

    target = tmp_path / "crates" / "x" / "target"
    probe = target / "debug" / "deps" / "probe-0123456789abcdef"
    reading = parse_footprint(
        "PID PPID ELAPSED %CPU RSS COMMAND\n"
        f"100 1 04:00:00 0.0 1024 {probe}\n"
    )
    # No CACHEDIR.TAG yet: the shape alone must not confirm.
    assert doctor_footprint._confirmed_orphans(reading) == []
    target.mkdir(parents=True)
    (target / "CACHEDIR.TAG").write_text(_CACHEDIR_TAG)
    confirmed = doctor_footprint._confirmed_orphans(reading)
    assert [o.pid for o in confirmed] == [100]


def test_reap_orphans_dry_run_names_and_spares(planted_orphan, monkeypatch):
    pid = planted_orphan
    monkeypatch.setenv("FNO_TEST_ORPHAN_MIN_ELAPSED_SECONDS", "0")
    result = runner.invoke(app, ["doctor", "footprint", "--reap-orphans", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    rows = [r for r in payload["orphan_test_binaries"] if r["pid"] == pid]
    assert len(rows) == 1, payload["orphan_test_binaries"]
    assert rows[0]["reaped"] is False
    assert "dry-run" in rows[0]["reason"]
    assert _pid_alive(pid), "a dry run must never signal"


def test_reap_orphans_apply_kills_the_planted_orphan(planted_orphan, monkeypatch):
    pid = planted_orphan
    monkeypatch.setenv("FNO_TEST_ORPHAN_MIN_ELAPSED_SECONDS", "0")
    result = runner.invoke(
        app, ["doctor", "footprint", "--reap-orphans", "--apply", "--json"]
    )

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    rows = [r for r in payload["orphan_test_binaries"] if r["pid"] == pid]
    assert len(rows) == 1, payload["orphan_test_binaries"]
    assert rows[0]["reaped"] is True
    # Dead for real, not just on paper.
    import time

    deadline = time.monotonic() + 5
    while _pid_alive(pid) and time.monotonic() < deadline:
        time.sleep(0.1)
    assert not _pid_alive(pid), "the planted orphan must be dead after --apply"


def test_reap_orphans_holds_a_fresh_orphan_below_the_guard(planted_orphan):
    pid = planted_orphan
    # No env override: the 900s default guard keeps a seconds-old plant alive.
    result = runner.invoke(
        app, ["doctor", "footprint", "--reap-orphans", "--apply", "--json"]
    )

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    rows = [r for r in payload["orphan_test_binaries"] if r["pid"] == pid]
    assert len(rows) == 1, payload["orphan_test_binaries"]
    assert rows[0]["reaped"] is False
    assert "guard" in rows[0]["reason"]
    assert _pid_alive(pid), "a below-guard orphan must be reported, never killed"


def test_orphan_binaries_reach_the_normal_json_payload(monkeypatch, tmp_path, no_worker_roots):
    """The ordinary footprint reading names confirmed orphans too, so the
    alarm fires even when nobody asked for the reap lever."""
    from fno import doctor_footprint

    target = tmp_path / "crates" / "x" / "target"
    probe = target / "debug" / "deps" / "probe-0123456789abcdef"
    target.mkdir(parents=True)
    (target / "CACHEDIR.TAG").write_text(_CACHEDIR_TAG)
    _pin_load(monkeypatch, status="within")
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            monkeypatch,
            "PID PPID ELAPSED %CPU RSS COMMAND\n"
            f"100 1 04:00:00 0.0 1024 {probe}\n"
            "200 1 01:00:00 20.0 1024 fno-agents-worker --run\n",
            [{"name": "worker-a"}],
            [],
        ),
    )

    result = runner.invoke(app, ["doctor", "footprint", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert [o["pid"] for o in payload["orphan_test_binaries"]] == [100]
    assert payload["orphan_test_binaries"][0]["zombies"] == 0
