"""Tests for the hidden ``fno doctor footprint`` diagnostic."""

from __future__ import annotations

import json
import os
import subprocess

import pytest
from typer.testing import CliRunner

from fno.cli import app


runner = CliRunner()


def _fake_runner(ps_output: str, roster: list[dict], calls: list[list[str]]):
    def run(argv, **kwargs):
        calls.append(list(argv))
        if argv[0] == "ps":
            kwargs["stdout"].write(ps_output)
            return subprocess.CompletedProcess(argv, 0)
        return subprocess.CompletedProcess(argv, 0, stdout=json.dumps(roster), stderr="")

    return run


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
        lambda: (None, "ps unavailable: timed out after 5.0s"),
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

    assert doctor_footprint._live_root_pids() == (
        set(),
        "worker root discovery unavailable",
    )


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

    assert doctor_footprint._live_root_pids() == (
        set(),
        "worker root discovery unavailable",
    )


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


def test_ac5_hp_json_reports_fleet_totals_and_cpu_shares(monkeypatch) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
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


def test_ac6_edge_cause_only_excludes_observer_subtree_and_skips_roster(monkeypatch) -> None:
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
    assert calls == [["ps", "-Ao", "pid,ppid,etime,%cpu,rss,command"]]


def test_ac6_edge_cause_only_seeds_live_detached_registry_root(monkeypatch) -> None:
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


def test_ac6_edge_cause_only_refuses_root_missing_from_snapshot(monkeypatch) -> None:
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


def test_ac7_edge_fleet_cpu_threshold_includes_short_lived_descendant(monkeypatch) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
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

    assert result.exit_code == 3, result.output
    assert "fleet CPU: 1.200 cores" in result.stdout
    assert "verdict: over budget" in result.stdout


def test_ac8_edge_descendants_do_not_consume_direct_process_threshold(monkeypatch) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
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

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 0, result.output
    assert "processes: 3" in result.stdout
    assert "direct processes: 1 (threshold 2)" in result.stdout


def test_ac9_edge_cpu_share_uses_constrained_capacity(monkeypatch) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(doctor_footprint.os, "cpu_count", lambda: 64)
    monkeypatch.setattr(doctor_footprint.os, "process_cpu_count", lambda: 2, raising=False)
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
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


def test_ac3_hp_reports_both_thresholds_and_exits_zero(monkeypatch) -> None:
    from fno import doctor_footprint

    calls: list[list[str]] = []
    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            """\
            PID ELAPSED %CPU RSS COMMAND
            101 01:00:00 20.0 1024 fno mux serve
            102 00:00:01 92.0 1024 fno --version
            """,
            [{"name": "worker-a"}, {"name": "worker-b"}],
            calls,
        ),
    )
    monkeypatch.setattr(doctor_footprint.shutil, "which", lambda name: "/usr/local/bin/fno")

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 0, result.output
    assert "sustained CPU: 0.200 cores (threshold 1.000)" in result.stdout
    assert "processes: 2" in result.stdout
    assert "direct processes: 2 (threshold 3)" in result.stdout
    assert "transient calls: 1" in result.stdout
    assert [call for call in calls if call[0] in {"ps", "/usr/local/bin/fno"}] == [
        ["ps", "-Ao", "pid,ppid,etime,%cpu,rss,command"],
        ["/usr/local/bin/fno", "agents", "list", "--status", "live", "--json"],
    ]


def test_ac4_edge_sustained_cpu_exits_three_and_names_top_consumers(monkeypatch) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            """\
            PID ELAPSED %CPU RSS COMMAND
            201 02:00:00 80.0 1024 fno mux serve
            202 01:00:00 40.0 2048 fno-agents-daemon --serve
            """,
            [],
            [],
        ),
    )
    monkeypatch.setattr(doctor_footprint.shutil, "which", lambda name: "/usr/local/bin/fno")

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 3
    assert "over budget" in result.stdout
    assert "fno mux serve (80.0%)" in result.stdout
    assert "fno-agents-daemon --serve (40.0%)" in result.stdout


def test_ac4_edge_process_count_also_exits_three(monkeypatch) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            """\
            PID ELAPSED %CPU RSS COMMAND
            211 02:00:00 10.0 1024 fno worker-a
            212 02:00:00 10.0 1024 fno worker-b
            """,
            [],
            [],
        ),
    )
    monkeypatch.setattr(doctor_footprint.shutil, "which", lambda name: "/usr/local/bin/fno")

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 3
    assert "sustained CPU: 0.200 cores" in result.stdout
    assert "processes: 2" in result.stdout
    assert "direct processes: 2 (threshold 1)" in result.stdout


def test_ac5_edge_roster_failure_exits_four_without_a_default_threshold(monkeypatch) -> None:
    from fno import doctor_footprint

    calls: list[list[str]] = []

    def failed_roster(argv, **kwargs):
        calls.append(list(argv))
        if argv[0] == "ps":
            kwargs["stdout"].write(
                "PID ELAPSED %CPU RSS COMMAND\n101 01:00:00 20.0 1024 fno daemon\n"
            )
            return subprocess.CompletedProcess(argv, 0)
        return subprocess.CompletedProcess(argv, 1, stdout="", stderr="roster failed")

    monkeypatch.setattr(doctor_footprint.subprocess, "run", failed_roster)
    monkeypatch.setattr(doctor_footprint.shutil, "which", lambda name: "/usr/local/bin/fno")

    result = runner.invoke(app, ["doctor", "footprint"])

    assert result.exit_code == 4
    assert "roster unavailable" in result.stdout
    assert "processes:" not in result.stdout
    assert calls[-1] == [
        "/usr/local/bin/fno",
        "agents",
        "list",
        "--status",
        "live",
        "--json",
    ]


def test_ac7_edge_json_contains_thresholds_and_exit_meaning(monkeypatch) -> None:
    from fno import doctor_footprint

    monkeypatch.setattr(
        doctor_footprint.subprocess,
        "run",
        _fake_runner(
            """\
            PID ELAPSED %CPU RSS COMMAND
            301 00:00:01 92.0 1024 fno --version
            """,
            [{"name": "worker-a"}],
            [],
        ),
    )
    monkeypatch.setattr(doctor_footprint.shutil, "which", lambda name: "/usr/local/bin/fno")

    result = runner.invoke(app, ["doctor", "footprint", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["sustained_cpu_cores"] == 0.0
    assert payload["transient_call_count"] == 1
    assert payload["sustained_cpu_threshold_cores"] == 1.0
    assert payload["direct_process_count_threshold"] == 2
    assert payload["exit_code"] == 0
