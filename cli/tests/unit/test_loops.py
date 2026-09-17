"""Tests for the Rust-owned global pause sentinel adapter."""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

runner = CliRunner()


@pytest.fixture
def isolated_home(tmp_path: Path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    yield tmp_path


def test_loop_level_unconfigured_defaults_to_report(isolated_home):
    from fno.loops import loop_level

    assert loop_level("nonexistent") == "report"


def test_loops_paused_reads_rust_verdict(isolated_home, monkeypatch):
    from fno import loops

    calls = []
    monkeypatch.setattr(
        loops,
        "_rust_loops_call",
        lambda action, args=None: calls.append((action, args)) or {
            "paused": True,
            "state": "paused",
            "who": "tester",
        },
    )

    assert loops.loops_paused() is True
    assert calls == [("paused", None)]


def test_pause_all_is_a_passthrough_to_the_rust_binary(isolated_home, monkeypatch, tmp_path):
    """AC8-HP: the stub receives the action + argv unchanged; stdout passes through."""
    from fno import loops

    stub = tmp_path / "fno-agents-stub"
    stub.write_text("#!/bin/sh\necho \"got: $@\"\n")
    stub.chmod(0o755)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: stub)

    result = runner.invoke(loops.loops_app, ["pause-all", "--ttl", "30m", "--reason", "talk"])
    assert result.exit_code == 0, result.output
    assert "got: loops pause-all --ttl 30m --reason talk" in result.output


def test_resume_all_is_a_passthrough_to_the_rust_binary(isolated_home, monkeypatch, tmp_path):
    from fno import loops

    stub = tmp_path / "fno-agents-stub"
    stub.write_text("#!/bin/sh\necho \"got: $@\"\nexit 7\n")
    stub.chmod(0o755)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: stub)

    result = runner.invoke(loops.loops_app, ["resume-all"])
    assert result.exit_code == 7, result.output
    assert "got: loops resume-all" in result.output


def test_expired_rust_verdict_is_not_paused(isolated_home, monkeypatch):
    from fno import loops

    monkeypatch.setattr(
        loops,
        "_rust_loops_call",
        lambda action, args=None: {
            "paused": False,
            "state": "expired",
            "who": "tester",
            "paused_at": 10,
        },
    )
    assert loops.loops_paused() is False


def test_bad_rust_result_fails_closed_and_names_binary(isolated_home, monkeypatch, caplog):
    from fno import loops

    def fail(*_args, **_kwargs):
        raise RuntimeError("fno-agents binary /tmp/fno-agents returned bad JSON")

    monkeypatch.setattr(loops, "_rust_loops_call", fail)
    assert loops.loops_paused() is True
    assert "/tmp/fno-agents" in caplog.text


def test_cli_status_reports_corrupt_state(isolated_home, monkeypatch):
    from fno import loops

    monkeypatch.setattr(
        loops,
        "_rust_loops_call",
        lambda action, args=None: {
            "paused": True,
            "state": "corrupt",
            "path": "/tmp/loops-paused.json",
        },
    )
    result = runner.invoke(loops.loops_app, ["status"])
    assert result.exit_code == 0, result.output
    assert "corrupted" in result.output
    assert "treated as paused" in result.output


def test_cli_ls_with_no_loops_configured(isolated_home):
    from fno.loops import loops_app

    result = runner.invoke(loops_app, ["ls"])
    assert result.exit_code == 0, result.output
    assert "no loops configured" in result.output


def test_cli_ls_lists_configured_loop_with_level(isolated_home, tmp_path, monkeypatch):
    settings_file = tmp_path / "settings.yaml"
    settings_file.write_text(
        "config:\n  loops:\n    my-loop:\n      level: assisted\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno.loops import loops_app

    result = runner.invoke(loops_app, ["ls"])
    assert result.exit_code == 0, result.output
    assert "my-loop" in result.output
    assert "assisted" in result.output
    assert "never" in result.output


def test_last_tick_survives_null_data_event(isolated_home, tmp_path, monkeypatch):
    import json as json_mod

    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    events_path = tmp_path / ".fno" / "events.jsonl"
    events_path.parent.mkdir(parents=True, exist_ok=True)
    events_path.write_text(
        json_mod.dumps({"ts": "2026-01-01T00:00:00Z", "type": "loop_tick", "data": None}) + "\n",
        encoding="utf-8",
    )
    from fno.loops import _last_tick

    assert _last_tick("my-loop") is None
