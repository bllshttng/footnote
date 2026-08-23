"""Tests for the hidden ``fno doctor footprint`` diagnostic."""

from __future__ import annotations

import json
import subprocess

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
    assert "processes: 2 (threshold 3)" in result.stdout
    assert "transient calls: 1" in result.stdout
    assert [call for call in calls if call[0] in {"ps", "/usr/local/bin/fno"}] == [
        ["ps", "-Ao", "pid,etime,%cpu,rss,command"],
        ["/usr/local/bin/fno", "agents", "list", "--json"],
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
    assert calls[-1] == ["/usr/local/bin/fno", "agents", "list", "--json"]


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
    assert payload["process_count_threshold"] == 2
    assert payload["exit_code"] == 0
