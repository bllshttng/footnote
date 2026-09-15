"""The evals tick phase: the seam between the tick and the native
evals-arm. The due read, the gate, the detached run and the journal are all
Rust and tested there; these tests pin the Python half: the guards, the argv,
the receipt-to-tick-row parse, and every failure shape landing as
``arm_failed`` without raising out of the tick.
"""
import json
import stat

import pytest

from fno.pr_watch import cli as pr_watch_cli


class _Evals:
    def __init__(self, schedule: int = 7, stale: int = 7) -> None:
        self.schedule_days = schedule
        self.stale_days = stale


class _Settings:
    def __init__(self, evals: object | None = None) -> None:
        self.evals = evals if evals is not None else _Evals()


@pytest.fixture()
def rows(monkeypatch):
    captured = []
    monkeypatch.setattr(pr_watch_cli, "_emit_tick_row", lambda *a, **kw: captured.append((a, kw)))
    return captured


@pytest.fixture()
def evals_world(monkeypatch, tmp_path):
    """A deterministic world: tmp history + summary, no real reads."""
    history = tmp_path / "history.jsonl"
    history.write_text("", encoding="utf-8")
    monkeypatch.setattr("fno.evals.report.evals_health_summary", lambda p, **kw: {"age_days": 9.0})
    monkeypatch.setattr("fno.paths.evals_history", lambda: history)
    return history


def _fake_binary(tmp_path, script):
    binary = tmp_path / "fake-fno-agents"
    binary.write_text(script)
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    return binary


# AC3-HP: the argv carries the arm name and every path flag; one tick row
# mirrors the receipt (acted 1, no skip_reason).
def test_receipt_becomes_one_tick_row(monkeypatch, tmp_path, rows, evals_world):
    recorded = tmp_path / "argv.txt"
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary",
        lambda: _fake_binary(
            tmp_path,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ARGV_RECORD\"\n"
            "echo '{\"acted\": 1, \"skip_reason\": null, \"detail\": \"launched pid 7\"}'\n",
        ),
    )
    monkeypatch.setenv("ARGV_RECORD", str(recorded))

    pr_watch_cli._run_evals_arm_phase(_Settings(), seconds_left_fn=lambda: 100.0)

    argv = recorded.read_text().splitlines()
    assert "evals-arm" in argv
    for flag in ("--history", "--events", "--fno-bin", "--summary-json"):
        assert flag in argv, flag
    fno_bin_value = argv[argv.index("--fno-bin") + 1]
    assert fno_bin_value, "--fno-bin must carry a value"
    assert argv[argv.index("--schedule-days") + 1] == "7"
    assert len(rows) == 1
    args, kwargs = rows[0]
    assert args == ("evals",)
    assert kwargs["interval_s"] == 7 * 86400
    assert kwargs["acted"] == 1
    assert kwargs["skip_reason"] is None
    assert kwargs["detail"] == "launched pid 7"


def test_summary_json_rides_the_argv(monkeypatch, tmp_path, rows, evals_world):
    recorded = tmp_path / "argv.txt"
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary",
        lambda: _fake_binary(
            tmp_path,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ARGV_RECORD\"\n"
            "echo '{\"acted\": 0, \"skip_reason\": \"fresh\", \"detail\": \"x\"}'\n",
        ),
    )
    monkeypatch.setenv("ARGV_RECORD", str(recorded))

    pr_watch_cli._run_evals_arm_phase(_Settings(), seconds_left_fn=lambda: 100.0)

    argv = recorded.read_text().splitlines()
    assert json.loads(argv[argv.index("--summary-json") + 1]) == {"age_days": 9.0}


# AC3-EDGE: schedule_days 0 - the binary is never called, the row reads evals_off.
def test_unarmed_schedule_is_evals_off(monkeypatch, tmp_path, rows):
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary",
        lambda: _fake_binary(tmp_path, "#!/bin/sh\nexit 0\n"),
    )
    pr_watch_cli._run_evals_arm_phase(_Settings(_Evals(schedule=0)), seconds_left_fn=lambda: 100.0)
    assert len(rows) == 1
    _, kwargs = rows[0]
    assert kwargs["skip_reason"] == "evals_off"
    assert kwargs["interval_s"] == 0


def test_autonomy_off_gates_before_the_binary(monkeypatch, tmp_path, rows):
    monkeypatch.setattr("fno.config.autonomy_master_enabled", lambda: False)
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary",
        lambda: (_ for _ in ()).throw(AssertionError("binary must not resolve")),
    )
    pr_watch_cli._run_evals_arm_phase(_Settings(), seconds_left_fn=lambda: 100.0)
    _, kwargs = rows[0]
    assert kwargs["skip_reason"] == "autonomy_off"


# AC3-ERR: a non-zero exit and an unparseable receipt both land as arm_failed;
# the tick goes on (the function returns, never raises).
def test_nonzero_and_unparseable_runs_are_arm_failed(monkeypatch, tmp_path, rows, evals_world):
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary",
        lambda: _fake_binary(tmp_path, "#!/bin/sh\necho boom >&2\nexit 2\n"),
    )
    pr_watch_cli._run_evals_arm_phase(_Settings(), seconds_left_fn=lambda: 100.0)
    _, kwargs = rows[0]
    assert kwargs["skip_reason"] == "arm_failed"
    assert "exited 2" in kwargs["detail"]

    rows.clear()
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary",
        lambda: _fake_binary(tmp_path, "#!/bin/sh\necho 'garbage'\n"),
    )
    pr_watch_cli._run_evals_arm_phase(_Settings(), seconds_left_fn=lambda: 100.0)
    _, kwargs = rows[0]
    assert kwargs["skip_reason"] == "arm_failed"


def test_absent_binary_is_arm_failed(monkeypatch, tmp_path, rows, evals_world):
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: None)
    pr_watch_cli._run_evals_arm_phase(_Settings(), seconds_left_fn=lambda: 100.0)
    _, kwargs = rows[0]
    assert kwargs["skip_reason"] == "arm_failed"
