"""Tests for the loops pause-all sentinel + level helper (x-ce71).

Covers the plan's three ACs: pause/resume round-trip, expired-TTL reads as
not-paused (status says "expired"), and an unconfigured loop name always
resolves to level "report" without raising.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

runner = CliRunner()


@pytest.fixture
def isolated_home(tmp_path: Path, monkeypatch):
    """Isolate the global ~/.fno sentinel + settings caches per test."""
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)

    from fno import config as config_mod
    from fno import paths as paths_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
    yield tmp_path
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()  # type: ignore[attr-defined]


def test_loop_level_unconfigured_defaults_to_report(isolated_home):
    from fno.loops import loop_level

    assert loop_level("nonexistent") == "report"


def test_pause_then_loops_paused_is_true(isolated_home):
    from fno.loops import loops_paused, pause_all

    pause_all(who="tester")
    assert loops_paused() is True


def test_resume_then_loops_paused_is_false(isolated_home):
    from fno.loops import loops_paused, pause_all, resume_all

    pause_all(who="tester")
    resume_all()
    assert loops_paused() is False


def test_resume_when_not_paused_reports_false(isolated_home):
    from fno.loops import resume_all

    assert resume_all() is False


def test_expired_ttl_reads_as_not_paused(isolated_home):
    from fno.loops import loops_paused, pause_all

    pause_all(who="tester", ttl_ms=1)
    import time

    time.sleep(0.01)
    assert loops_paused() is False


def test_status_reports_expired_for_stale_ttl(isolated_home):
    from fno.loops import is_expired, pause_all, read_pause_state

    pause_all(who="tester", ttl_ms=1)
    import time

    time.sleep(0.01)
    state = read_pause_state()
    assert state is not None
    assert is_expired(state) is True


def test_cli_pause_status_resume_round_trip(isolated_home):
    from fno.loops import loops_app

    result = runner.invoke(loops_app, ["pause-all", "--who", "cli-tester"])
    assert result.exit_code == 0, result.output
    assert "cli-tester" in result.output

    result = runner.invoke(loops_app, ["status"])
    assert result.exit_code == 0, result.output
    assert "cli-tester" in result.output

    result = runner.invoke(loops_app, ["resume-all"])
    assert result.exit_code == 0, result.output
    assert "resumed" in result.output

    result = runner.invoke(loops_app, ["status"])
    assert result.exit_code == 0, result.output
    assert "not paused" in result.output


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

    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.loops import loops_app

    result = runner.invoke(loops_app, ["ls"])
    assert result.exit_code == 0, result.output
    assert "my-loop" in result.output
    assert "assisted" in result.output
    assert "never" in result.output  # no loop_tick events yet
