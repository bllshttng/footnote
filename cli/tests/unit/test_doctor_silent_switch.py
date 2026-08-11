"""Tests for `fno doctor` silent-switch legibility (x-8cd5 Wave 6).

The rule, both directions: any default-off switch that can silently produce
inaction owes a doctor line; any default-on switch that can silently take an
irreversible action owes one too. Advisory only, never changes the exit code.
"""
from __future__ import annotations

import types

import pytest
from typer.testing import CliRunner

from fno import doctor
from fno.cli import app

runner = CliRunner()


def _fake_settings(
    *,
    active_backlog: bool = False,
    think_spawn: bool = False,
    auto_merge: bool = False,
    dispatch_am: bool = False,
) -> types.SimpleNamespace:
    return types.SimpleNamespace(
        active_backlog=types.SimpleNamespace(enabled=active_backlog),
        think_spawn=types.SimpleNamespace(enabled=think_spawn),
        auto_merge=types.SimpleNamespace(enabled=auto_merge),
        dispatch=types.SimpleNamespace(auto_merge=dispatch_am),
    )


def _patch_silent(
    monkeypatch: pytest.MonkeyPatch,
    *,
    settings: types.SimpleNamespace,
    missions: int = 0,
    armed: int = 0,
) -> None:
    monkeypatch.setattr("fno.config.load_settings", lambda: settings)
    monkeypatch.setattr(doctor, "_mission_active_count", lambda: missions)
    monkeypatch.setattr(doctor, "_auto_merge_armed_manifests", lambda: armed)
    monkeypatch.setattr(doctor, "_read_posture_stamp", lambda: None)


# ---------------------------------------------------------------------------
# Collector: both directions
# ---------------------------------------------------------------------------


def test_drain_off_with_missions_named_as_inaction(monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(active_backlog=False),
        missions=5,
    )
    report = doctor._silent_switch_report()
    sw = [f for f in report["findings"] if f["switch"] == "active_backlog.enabled"]
    assert len(sw) == 1
    assert sw[0]["direction"] == "inaction"
    assert sw[0]["count"] == 5
    assert sw[0]["command"] == "fno config set active_backlog.enabled true"


def test_drain_off_with_no_missions_is_not_inaction(monkeypatch: pytest.MonkeyPatch) -> None:
    """A default-off switch with nothing to drain is a default, not inaction."""
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(active_backlog=False),
        missions=0,
    )
    report = doctor._silent_switch_report()
    assert not any(f["switch"] == "active_backlog.enabled" for f in report["findings"])


def test_think_spawn_off_named_as_inaction(monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_silent(monkeypatch, settings=_fake_settings(think_spawn=False))
    report = doctor._silent_switch_report()
    ts = [f for f in report["findings"] if f["switch"] == "think_spawn.enabled"]
    assert len(ts) == 1
    assert ts[0]["direction"] == "inaction"
    assert "think_spawn.enabled true" in ts[0]["command"]


def test_auto_merge_armed_named_as_irreversible(monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(auto_merge=True, dispatch_am=True),
        armed=3,
    )
    report = doctor._silent_switch_report()
    irreversible = [f for f in report["findings"] if f["direction"] == "irreversible"]
    switches = {f["switch"] for f in irreversible}
    assert "auto_merge.enabled" in switches
    assert "dispatch.auto_merge" in switches
    assert "auto_merge_approved (worktree manifests)" in switches
    by_sw = {f["switch"]: f for f in irreversible}
    assert by_sw["auto_merge_approved (worktree manifests)"]["count"] == 3
    # Every irreversible finding carries a disarm command.
    for f in irreversible:
        assert "false" in f["command"]


def test_both_directions_when_drain_off_and_merge_armed(monkeypatch: pytest.MonkeyPatch) -> None:
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(active_backlog=False, auto_merge=True, dispatch_am=True),
        missions=2,
        armed=1,
    )
    report = doctor._silent_switch_report()
    directions = {f["direction"] for f in report["findings"]}
    assert directions == {"inaction", "irreversible"}


# ---------------------------------------------------------------------------
# Behavioural: the doctor command names the switch in its output
# ---------------------------------------------------------------------------


def _stub_quiet_machine(monkeypatch: pytest.MonkeyPatch) -> None:
    """Silence the expensive doctor collectors so the verdict is 'fresh' and the
    silent-switch lines are the interesting output."""
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: None)
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: "present")
    monkeypatch.setattr(doctor, "_python_content_drift", lambda source: 0)
    monkeypatch.setattr(
        doctor, "_rust_report", lambda: {"binary": None, "revision": None}
    )
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: False)
    monkeypatch.setattr(
        doctor, "_groom_health", lambda: {"state": "ran", "hours": 3.0, "stale": False, "agent_installed": True}
    )
    monkeypatch.setattr(doctor, "_launch_agent_failures", lambda: {"applicable": True, "dead": []})


def test_doctor_names_drain_off_with_missions(monkeypatch: pytest.MonkeyPatch) -> None:
    """Verify behaviourally: drain disabled + >=1 mission active -> doctor names it."""
    _stub_quiet_machine(monkeypatch)
    _patch_silent(
        monkeypatch,
        settings=_fake_settings(active_backlog=False, think_spawn=False, auto_merge=True, dispatch_am=True),
        missions=5,
        armed=31,
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, result.output
    out = result.output
    assert "active_backlog.enabled is OFF but 5 mission_active epic(s) queued waiting" in out
    assert "fno config set active_backlog.enabled true" in out
    assert "think_spawn.enabled is OFF" in out
    # Both directions in one report.
    assert "auto_merge.enabled is ARMED" in out
    assert "dispatch.auto_merge is ARMED" in out
    assert "31 manifest(s)" in out
    # Advisory: does not change the exit code.
    assert "up to date" in out or "status unknown" in out
