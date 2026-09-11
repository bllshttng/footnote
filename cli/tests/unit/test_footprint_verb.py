"""Tests for the hidden ``fno doctor footprint`` diagnostic."""

from __future__ import annotations

import json
import os
import subprocess

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
    monkeypatch.setattr(
        doctor_footprint,
        "_codex_app_server_serve",
        lambda _snapshot: (set(), "absent"),
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


def _pin_load(
    monkeypatch,
    *,
    status: str,
    load: float = 1.0,
    ceiling: float = 96.0,
    load_15m: float | None = None,
):
    """Pin the spawn-load snapshot so a verdict test is hermetic: the real
    snapshot reads the host's live load average, which no exit-code assertion
    should ride on. The 15-minute figure feeds the CPU axis's backstop; the
    1-minute load is display-only under x-7783."""
    from types import SimpleNamespace

    from fno import doctor_footprint

    snapshot = SimpleNamespace(
        load_1m=load,
        max_load_per_cpu=8.0,
        load_ceiling=ceiling,
        load_cpu_count=int(ceiling // 8),
        spawn_load_status=status,
        load_5m=None,
        load_15m=load_15m,
    )
    monkeypatch.setattr(doctor_footprint, "_spawn_load_snapshot", lambda: snapshot)


def _pin_admission(monkeypatch, share: float = 0.5, hard: float = 40.0):
    """Pin the CPU axis's config pair so a verdict test never reads the real
    config roots (the defaults match a stock install)."""
    from fno import doctor_footprint

    monkeypatch.setattr(doctor_footprint, "_admission_config", lambda: (share, hard))


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
    refusing were exactly this population - claims expired, rows still live.
    The harness is one with no shared daemon, so the row rides the per-row
    witness path (x-cb2b moved pidless codex rows to the daemon verdict)."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="luna",
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


def _codex_thread_row(name: str, session_id: str | None = None):
    from types import SimpleNamespace

    return SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="codex",
        short_id="",
        name=name,
        harness_session_id=session_id,
    )


def test_row_is_advancing_reads_the_shared_progress_classifier(monkeypatch) -> None:
    """x-9958 Task 2: the discriminator is classify_progress's own verdict -
    advancing on transcript-turn, a working reading inside STALE_ATTENTION_S.
    A silent or unreadable probe is never advancing."""
    from fno import doctor_footprint
    import fno.agents.session_truth as session_truth

    row = _codex_thread_row("t-probe-row", "tid-1")
    answer: dict = {}

    def fake_truth(handle, **kwargs):
        return dict(answer)

    monkeypatch.setattr(session_truth, "resolve_session_truth", fake_truth)

    answer.update(
        {"state": "working", "last_activity_age_s": 30, "observed_model": None}
    )
    assert doctor_footprint._row_is_advancing(row) is True

    answer.update({"state": "unknown", "reason": "not-found", "last_activity_age_s": None})
    assert doctor_footprint._row_is_advancing(row) is False

    def crashed_truth(handle, **kwargs):
        raise RuntimeError("unreadable store")

    monkeypatch.setattr(session_truth, "resolve_session_truth", crashed_truth)
    assert doctor_footprint._row_is_advancing(row) is False


def test_live_root_pids_resolves_a_codex_thread_row_through_its_rollout(
    monkeypatch,
) -> None:
    """x-9958 Task 3: a codex thread row's session id has an accepting route -
    the rollout fd - and a resolved, live pid attributes like any root."""
    from fno import doctor_footprint

    row = _codex_thread_row("t-codex-thread", "tid-907")
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.codex_rollout_pid_map",
        lambda session_ids, **kwargs: {"tid-907": 907},
    )
    monkeypatch.setattr(doctor_footprint, "_root_pid_is_live", lambda pid, start: True)

    assert doctor_footprint._live_root_pids() == ({907}, None)


def test_live_root_pids_keeps_an_unresolved_codex_row_as_a_named_gap(monkeypatch) -> None:
    """One oracle answered nothing: fail closed, the row stays a NAMED gap -
    a rollout miss proves nothing, so it never corpse-drops."""
    from fno import doctor_footprint

    row = _codex_thread_row("t-codex-thread", "tid-907")
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.codex_rollout_pid_map", lambda session_ids, **kwargs: {}
    )
    monkeypatch.setattr(doctor_footprint, "_claim_witness", lambda _name: "live")

    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)
    assert "t-codex-thread" in error.text


def test_live_root_pids_refuses_a_resolved_codex_root_that_is_dead(monkeypatch) -> None:
    """A rollout pid that died between the walk and the liveness check is the
    same hard unreadable the claude routed arm refuses on."""
    from fno import doctor_footprint

    row = _codex_thread_row("t-codex-thread", "tid-907")
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.session_procs.codex_rollout_pid_map",
        lambda session_ids, **kwargs: {"tid-907": 907},
    )
    monkeypatch.setattr(doctor_footprint, "_root_pid_is_live", lambda pid, start: False)

    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert error == "worker root liveness unavailable"


def test_live_root_pids_spares_an_advancing_row_and_names_only_the_silent_one(
    monkeypatch,
) -> None:
    """x-9958 Task 2: a pidless row advancing by transcript evidence is a live
    worker, not an unattributable process - it drops from the gap and the
    reading stands as an undercount. Positive marker: the silent sibling is
    still named, and only the silent row was ever witnessed."""
    from fno import doctor_footprint
    import fno.agents.session_truth as session_truth

    advancing = _codex_thread_row("t-adv-row")
    silent = _codex_thread_row("t-silent-row")
    monkeypatch.setattr(
        "fno.agents.registry.load_registry", lambda: [advancing, silent]
    )

    def fake_truth(handle, **kwargs):
        if handle == "t-adv-row":
            return {
                "state": "working",
                "last_activity_age_s": 30,
                "observed_model": None,
            }
        return {
            "state": "unknown",
            "reason": "not-found",
            "last_activity_age_s": None,
            "observed_model": {"kind": "no-transcript"},
        }

    monkeypatch.setattr(session_truth, "resolve_session_truth", fake_truth)

    witness_calls: list[str] = []
    monkeypatch.setattr(
        doctor_footprint,
        "_claim_witness",
        lambda name: witness_calls.append(name) or "live",
    )

    roots, error = doctor_footprint._live_root_pids()
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)
    assert "t-silent-row" in error.text
    assert "t-adv-row" not in error.text
    assert witness_calls == ["t-silent-row"]


def test_live_root_pids_spends_no_advancing_probes_on_a_spent_deadline(monkeypatch) -> None:
    """The advancing pass is a budget consumer: once the reading's deadline is
    gone, probes stop and the rows fall to the witness, which fails closed."""
    import time as time_mod

    from fno import doctor_footprint

    row = _codex_thread_row("t-late-row")
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])

    def refused(*args, **kwargs):
        raise AssertionError("advancing probe ran on a spent deadline")

    monkeypatch.setattr(doctor_footprint, "_row_is_advancing", refused)

    roots, error = doctor_footprint._live_root_pids(deadline=time_mod.monotonic() - 1)
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)


def test_live_root_pids_pane_row_costs_the_attributed_mux_server(monkeypatch) -> None:
    """A pane burns CPU inside the mux server process the reading attributes;
    whatever the probe answers, the pane adds no unattributed cost. Only an
    answer the mux could NOT give leaves the cost unproven. The harness is
    one with no shared daemon, so the row rides the pane-probe path (x-cb2b
    moved pidless codex rows to the daemon verdict)."""
    from fno import doctor_footprint
    from types import SimpleNamespace

    row = SimpleNamespace(
        status="live",
        pid=None,
        pid_start_time=None,
        harness="luna",
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
        load_cpu_count=12,
        load_5m=141.0,
        load_15m=140.0,
    )
    monkeypatch.setattr("fno.config.load_settings", lambda: settings)
    monkeypatch.setattr(spawn_gate, "_load_snapshot", lambda _factor: snapshot)
    monkeypatch.setattr(doctor_footprint, "_cpu_capacity_cores", lambda: 12)

    payload = doctor_footprint._payload(
        reading, process_threshold=None, exit_code=0
    )

    assert payload["load_1m"] == pytest.approx(141.6)
    assert payload["load_cpu_count"] == 12
    assert "max_load_per_cpu" not in payload
    assert "spawn_load_status" not in payload

    with pytest.raises(typer.Exit):
        doctor_footprint._emit_result(
            reading, process_threshold=None, json_output=False
        )
    out = capsys.readouterr().out
    # x-7783 AC8: one cpu admission line, one load_15m line, no spawn load.
    assert (
        "cpu admission: fleet 0.490 of 12.00 cores (4.1%) against "
        "max_fleet_cpu_share 50.0% -> admit" in out
    )
    assert (
        "load_15m: 140.0 against backstop 480.0 "
        "(hard_max_load_per_cpu 40 x 12 cpus)" in out
    )
    assert "spawn load:" not in out


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
    _pin_admission(monkeypatch)
    _pin_capacity(monkeypatch, 12)
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
    assert "verdict: admit on fleet_cpu_share (10.0% against 50.0%)" in result.stdout


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
    """The backstop over its ceiling and a leak BOTH fire; the CPU axis keeps
    the exit (3) as the more urgent alarm and the leak still prints with its
    own words. The 1-minute load pinned beside it decides nothing (x-7783)."""
    from fno import doctor_footprint

    _pin_load(monkeypatch, status="within", load=110.4, load_15m=500.0)
    _pin_admission(monkeypatch)
    _pin_capacity(monkeypatch, 12)
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
    assert "verdict: refuse on load_15m (500.0 against 480.0)" in result.stdout
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
    _pin_admission(monkeypatch)
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
    assert "verdict: leak on fleet_cpu_share (5.0% against 50.0%)" in result.stdout
    assert "sustained CPU: 0.200 cores" in result.stdout
    assert "processes: 2" in result.stdout
    assert "unexplained processes: 1 (2 direct, roster explains 1)" in result.stdout


def test_ac5_edge_roster_failure_degrades_the_threshold_not_the_reading(
    monkeypatch, no_worker_roots, tmp_path
) -> None:
    """x-e040: the roster is an enrichment. On roster failure the measurement
    still prints, with the threshold degraded away and the reason named. The
    old contract killed the whole report (exit 4, no reading)."""
    # A cold HOME sends config resolution climbing to the canonical root,
    # whose resolver shells `git worktree list` through the SAME global
    # subprocess module this test pins. Pin the root so the startup probe is
    # an env read, and the pinned budget below stays the verdict's alone.
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
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

    # The CLI app resolves config roots through git when the process cache is
    # cold, and the admission pair and CPU override read config per verdict.
    # Warm the cache and pin the seams before the recorder goes in, so the
    # recorded window holds only what a footprint run itself executes.
    import contextlib

    from fno.config import load_settings

    with contextlib.suppress(Exception):
        load_settings()
    _pin_admission(monkeypatch)
    monkeypatch.setattr(doctor_footprint, "_footprint_cpu_override", lambda: None)
    monkeypatch.setattr(doctor_footprint.subprocess, "run", ps_only)
    monkeypatch.setattr("fno.agents.registry.load_registry", unreadable_registry)
    # ps is the only subprocess this report may spend. The roster is not the
    # only enrichment anymore: repo-root and worktree attribution also shell
    # out when their declarations are cold, so pin both seams hermetic.
    monkeypatch.setenv("FNO_REPO_ROOT", str(os.getcwd()))
    import fno.paths as _paths

    monkeypatch.setattr(_paths, "resolve_canonical_worktree", lambda *a, **k: None)
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
    assert payload["capacity_verdict"] == "admit"
    assert payload["admission"]["verdict"] == "admit"
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


def test_codex_app_server_serve_attributes_a_live_root(monkeypatch, tmp_path) -> None:
    """x-cb2b: the fno-harness-daemon state file names a live root; that pid
    joins the attributed roots and the verdict reads live."""
    from fno import doctor_footprint

    codex_home = tmp_path / "codex"
    (codex_home / "app-server-daemon").mkdir(parents=True)
    (codex_home / "app-server-daemon" / "fno-harness-daemon.json").write_text(
        json.dumps({"pid": 910, "processStartToken": 555}), encoding="utf-8"
    )
    monkeypatch.setenv("CODEX_HOME", str(codex_home))
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda pid, _start: True if pid == 910 else None,
    )

    assert doctor_footprint._codex_app_server_serve(set()) == ({910}, "live")


def test_codex_app_server_serve_accepts_alternate_token_spellings(
    monkeypatch, tmp_path
) -> None:
    """The Rust reader (codex_inject.rs parse_state) tolerates several token
    spellings; the Python reader answers the same words, so one provider
    spelling variant cannot gap the fleet."""
    from fno import doctor_footprint

    codex_home = tmp_path / "codex"
    (codex_home / "app-server-daemon").mkdir(parents=True)
    (codex_home / "app-server-daemon" / "fno-harness-daemon.json").write_text(
        json.dumps({"pid": 913, "process_start_time": 557}), encoding="utf-8"
    )
    monkeypatch.setenv("CODEX_HOME", str(codex_home))
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda pid, _start: True if pid == 913 else None,
    )

    assert doctor_footprint._codex_app_server_serve(set()) == ({913}, "live")


def test_codex_app_server_serve_falls_back_to_the_provider_pid_file(
    monkeypatch, tmp_path
) -> None:
    """No fno state file, but the provider's own app-server.pid names a live
    root: the fallback oracle attributes it, liveness proven without a token."""
    from fno import doctor_footprint

    codex_home = tmp_path / "codex"
    (codex_home / "app-server-daemon").mkdir(parents=True)
    (codex_home / "app-server-daemon" / "app-server.pid").write_text(
        json.dumps({"pid": 912}), encoding="utf-8"
    )
    monkeypatch.setenv("CODEX_HOME", str(codex_home))
    monkeypatch.setattr(
        "fno.agents.spawn_gate._pid_alive",
        lambda pid, _start: True if pid == 912 else None,
    )

    assert doctor_footprint._codex_app_server_serve(set()) == ({912}, "live")


def test_codex_app_server_serve_survives_a_malformed_state(monkeypatch, tmp_path) -> None:
    """x-cb2b: a malformed codex state file with no readable fallback never
    kills the reading; the serve answers unreadable and the caller degrades
    its rows to a gap."""
    from fno import doctor_footprint

    codex_home = tmp_path / "codex"
    (codex_home / "app-server-daemon").mkdir(parents=True)
    (codex_home / "app-server-daemon" / "fno-harness-daemon.json").write_text(
        "{not json", encoding="utf-8"
    )
    monkeypatch.setenv("CODEX_HOME", str(codex_home))

    assert doctor_footprint._codex_app_server_serve(set()) == (set(), "unreadable")


def test_pidless_codex_row_rides_a_live_shared_daemon(monkeypatch):
    """x-cb2b, the falsifiable discriminator: one live pidless codex row with
    a live app-server verdict is ATTRIBUTED, not a gap, so the refusal this
    node names (a later spawn denied while naming that row) cannot fire."""
    from fno import doctor_footprint

    def refused(*args, **kwargs):
        raise AssertionError("a live daemon verdict must not spend the row chain")

    monkeypatch.setattr(
        "fno.agents.registry.load_registry", lambda: [_pidless_row("codex")]
    )
    monkeypatch.setattr(doctor_footprint, "_row_is_advancing", refused)
    monkeypatch.setattr(doctor_footprint, "_claim_witness", refused)
    roots, error = doctor_footprint._live_root_pids(
        serves={"codex-app-server": "live"}
    )
    assert roots == set()
    assert error is None


def test_pidless_codex_row_stays_a_gap_under_an_unreadable_daemon(monkeypatch):
    """Fail closed: with the daemon verdict unreadable the row falls through
    to the per-row chain, and with no rollout, no advancing evidence and a
    live claim witness it lands back in the named gap sentence."""
    from fno import doctor_footprint

    monkeypatch.setattr(
        "fno.agents.registry.load_registry", lambda: [_pidless_row("codex")]
    )
    monkeypatch.setattr(doctor_footprint, "_row_is_advancing", lambda _row: False)
    monkeypatch.setattr(doctor_footprint, "_claim_witness", lambda _name: "live")
    roots, error = doctor_footprint._live_root_pids(
        serves={"codex-app-server": "unreadable"}
    )
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)
    assert "codex" in error.text
    assert "w1 (node=unknown)" in error.text


def test_pidless_unknown_harness_row_gains_nothing_from_daemon_verdicts(monkeypatch):
    """A harness with no shared daemon still gaps regardless of verdict, so
    the codex attribution buys no other row class a free pass."""
    from fno import doctor_footprint

    monkeypatch.setattr(
        "fno.agents.registry.load_registry", lambda: [_pidless_row("luna")]
    )
    monkeypatch.setattr(doctor_footprint, "_claim_witness", lambda _name: "live")
    roots, error = doctor_footprint._live_root_pids(
        serves={"codex-app-server": "live"}
    )
    assert roots == set()
    assert isinstance(error, doctor_footprint.AttributionGap)
    assert "luna" in error.text


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


def test_gap_reading_prints_the_measurement_and_admits_on_the_upper_bound(monkeypatch):
    """x-7783 LD3: an attribution gap no longer forces exit 4. The reading
    stands, the share becomes an interval, and a ceiling above the interval
    admits with `bound` recording that the upper edge decided. Both gates
    read the admission object, not the exit code."""
    from fno import doctor_footprint

    reading = doctor_footprint.parse_footprint(
        "PID PPID ELAPSED %CPU RSS COMMAND\n100 1 01:00:00 0.5 1024 fno daemon\n",
        excluded_root_pids=set(),
        attributed_root_pids=set(),
        threshold_excluded_root_pids=set(),
    )._replace(
        attribution_gap="1 pidless codex row(s) unresolved",
        measured_cpu_cores=0.02,
    )
    monkeypatch.setattr(
        doctor_footprint, "cause_reading", lambda: (reading, None)
    )
    _pin_load(monkeypatch, status="within", load_15m=2.0)
    _pin_admission(monkeypatch)
    _pin_capacity(monkeypatch, 12)
    result = runner.invoke(app, ["doctor", "footprint", "--json", "--cause-only"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["process_count"] >= 1
    assert "codex" in payload["attribution_gap"]
    assert payload["admission"]["verdict"] == "admit"
    assert payload["admission"]["bound"] == "upper"
    assert payload["exit_code"] == 0


def test_cause_only_reports_a_real_capacity_verdict(monkeypatch):
    """x-a457's done probe, carried onto the new axis: a clean cause-only
    reading answers the admission question instead of a structural unknown,
    and `capacity_verdict` aliases the admission verdict for one release."""
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
    _pin_admission(monkeypatch)
    _pin_capacity(monkeypatch, 12)
    result = runner.invoke(app, ["doctor", "footprint", "--json", "--cause-only"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["capacity_verdict"] == "admit"
    assert payload["admission"]["axis"] == "fleet_cpu_share"
    assert "attribution_gap" not in payload
    assert payload["exit_code"] == 0


def test_spawn_gate_carries_a_gap_reading_into_the_interval(monkeypatch):
    """x-7783 LD3: a gap no longer voids the reading. The gate's prefetch
    hands the gapped reading to the decider, and the share becomes an
    interval - never a bare None, never silent headroom."""
    from fno import doctor_footprint
    from fno.agents import spawn_gate

    reading = doctor_footprint.parse_footprint(
        "PID PPID ELAPSED %CPU RSS COMMAND\n100 1 01:00:00 30.0 1024 fno daemon\n",
        excluded_root_pids=set(),
        attributed_root_pids=set(),
        threshold_excluded_root_pids=set(),
    )._replace(
        attribution_gap="1 pidless codex row(s) unresolved",
        measured_cpu_cores=0.3,
    )
    monkeypatch.setattr(
        "fno.doctor_footprint.cause_reading", lambda: (reading, None)
    )
    monkeypatch.setattr(doctor_footprint, "_admission_config", lambda: (0.5, 40.0))
    monkeypatch.setattr(spawn_gate, "_load_cpus", lambda: 12)
    monkeypatch.setattr(spawn_gate.os, "getloadavg", lambda: (1.0, 1.0, 1.0))

    got_reading, error = spawn_gate._prefetch_fleet_reading()
    assert error is None and got_reading is reading

    admission = spawn_gate._cpu_axis((got_reading, error))
    assert admission.bound == "upper"
    assert admission.verdict in ("admit", "hold", "undecidable")
    assert admission.gap is not None


def test_admission_names_its_axis_and_deciding_numbers(monkeypatch):
    """AC7's naming contract (x-5283) carried onto the new axis (x-7783):
    the 15-minute backstop over its ceiling names load_15m as its axis and
    prints the numbers that decided it, and the sustained line disclaims the
    verdict. No one-minute figure appears on the deciding line."""
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
    _pin_load(monkeypatch, status="within", load=110.4, load_15m=500.0)
    _pin_admission(monkeypatch)
    _pin_capacity(monkeypatch, 12)
    result = runner.invoke(app, ["doctor", "footprint", "--json", "--cause-only"])
    payload = json.loads(result.stdout)
    assert payload["capacity_verdict"] == "refuse"
    assert payload["admission"]["axis"] == "load_15m"
    assert payload["admission"]["load_15m"] == 500.0
    assert payload["admission"]["backstop"] == 480.0
    assert payload["load_1m"] == 110.4

    shown = runner.invoke(app, ["doctor", "footprint", "--cause-only"])
    assert "verdict: refuse on load_15m (500.0 against 480.0)" in shown.output
    assert "a separate axis - it did not decide the verdict" in shown.output


def test_cpu_admission_pins_the_shared_gate_fixture():
    """x-7783 AC9: the four payloads both gates consume. The Python decider
    reproduces every admission from the case inputs; the Rust suite reads the
    same file and must take the same branch per payload."""
    from pathlib import Path

    from fno import doctor_footprint
    from fno.footprint import Footprint

    fixture_path = (
        Path(__file__).parent.parent / "agents" / "fixtures" / "spawn_gate_admission.json"
    )
    fixture = json.loads(fixture_path.read_text(encoding="utf-8"))
    assert len(fixture["cases"]) == 4
    for case in fixture["cases"]:
        inputs = case["inputs"]
        reading = Footprint(
            sustained_cpu_cores=0.0,
            descendant_cpu_cores=0.0,
            fleet_cpu_cores=inputs["fleet_cpu_cores"],
            descendant_process_count=0,
            direct_process_count=0,
            transient_call_count=0,
            process_count=0,
            rss_gb=0.0,
            measured_cpu_cores=inputs["measured_cpu_cores"],
            top=[],
            unparsed_lines=0,
            attribution_gap=inputs["attribution_gap"],
        )
        adm = doctor_footprint.cpu_admission(
            reading,
            capacity_cores=inputs["capacity_cores"],
            share_ceiling=inputs["share_ceiling"],
            load_15m=inputs["load_15m"],
            hard_max_load_per_cpu=inputs["hard_max_load_per_cpu"],
            cpus=inputs["cpus"],
        )
        expected = case["payload"]["admission"]
        assert adm.verdict == expected["verdict"], case["name"]
        assert adm.axis == expected["axis"], case["name"]
        assert adm.bound == expected["bound"], case["name"]
        assert adm.reason == expected["reason"], case["name"]
        assert adm.share_low == pytest.approx(expected["share_low"]), case["name"]
        assert adm.share_high == pytest.approx(expected["share_high"]), case["name"]


# ---------------------------------------------------------------------------
