"""Unit tests for `fno doctor --cost-check` (ab-c0f92987, US5).

Covers AC5-HP (agreement), AC5-ERR (divergence -> WARN, exit 1), AC5-UI
(reference tool absent -> skipped, exit 0), AC5-EDGE (no recent session with a
surviving transcript -> skipped), and AC5-FR (reference tool errors mid-run ->
skipped-with-reason, never a crash or false WARN).

The collectors (_find_recent_session_with_transcript, _run_session_cost,
_run_ref_cost_tool) are module-level so each test stubs them for a hermetic,
network-free run - same style as test_doctor.py's _stub_signals.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import doctor
from fno.cli import app

runner = CliRunner()

SID = "11111111-2222-3333-4444-555555555555"


def _stub_cost_signals(
    monkeypatch: pytest.MonkeyPatch,
    *,
    found: tuple[str, Path] | None = (SID, Path("/t.jsonl")),
    ours: float | None = 10.0,
    ref: tuple[float | None, str | None] = (10.0, None),
) -> None:
    monkeypatch.setattr(doctor, "_find_recent_session_with_transcript", lambda: found)
    monkeypatch.setattr(doctor, "_run_session_cost", lambda sid: ours)
    monkeypatch.setattr(doctor, "_run_ref_cost_tool", lambda sid: ref)


# --- AC5-HP: agreement -------------------------------------------------------


def test_ac5_hp_agreement_reports_ok(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_cost_signals(monkeypatch, ours=31.30, ref=(31.05, None))
    result = runner.invoke(app, ["doctor", "--cost-check"])
    assert result.exit_code == 0
    assert "cost-check OK" in result.stdout
    assert "$31.30" in result.stdout
    assert "$31.05" in result.stdout
    assert SID in result.stdout


def test_ac5_hp_exact_agreement(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_cost_signals(monkeypatch, ours=5.0, ref=(5.0, None))
    result = runner.invoke(app, ["doctor", "--cost-check"])
    assert result.exit_code == 0
    assert "divergence=0.0%" in result.stdout


# --- AC5-ERR: divergence -> WARN, nonzero ------------------------------------


def test_ac5_err_divergence_warns_and_exits_nonzero(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The original bug: our math 7.5x the reference tool's number.
    _stub_cost_signals(monkeypatch, ours=234.91, ref=(31.30, None))
    result = runner.invoke(app, ["doctor", "--cost-check"])
    assert result.exit_code == 1
    assert "cost-check WARN" in result.stdout
    assert "$234.91" in result.stdout
    assert "$31.30" in result.stdout


def test_boundary_divergence_at_threshold_is_ok(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Exactly 10% divergence is OK (threshold is "> 10%", per the plan).
    _stub_cost_signals(monkeypatch, ours=11.0, ref=(10.0, None))
    result = runner.invoke(app, ["doctor", "--cost-check"])
    assert result.exit_code == 0
    assert "cost-check OK" in result.stdout


def test_ref_zero_but_ours_nonzero_warns(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_cost_signals(monkeypatch, ours=4.0, ref=(0.0, None))
    result = runner.invoke(app, ["doctor", "--cost-check"])
    assert result.exit_code == 1
    assert "cost-check WARN" in result.stdout


# --- AC5-UI: absent reference tool ----------------------------------------------


def test_ac5_ui_ref_absent_skips_exit_zero(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_cost_signals(monkeypatch, ref=(None, "reference cost tool not installed"))
    result = runner.invoke(app, ["doctor", "--cost-check"])
    assert result.exit_code == 0
    assert "skipped (reference cost tool not installed)" in result.stdout


def test_real_run_ref_lookup_does_not_crash(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Unstubbed _run_ref_cost_tool with the reference tool absent from PATH
    # -> skip reason.
    monkeypatch.setattr(doctor.shutil, "which", lambda name: None)
    cost, reason = doctor._run_ref_cost_tool(SID)
    assert cost is None
    assert reason == "reference cost tool not installed"


# --- AC5-EDGE: no recent session with transcript --------------------------------


def test_ac5_edge_no_session_skips_with_reason(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_cost_signals(monkeypatch, found=None)
    result = runner.invoke(app, ["doctor", "--cost-check"])
    assert result.exit_code == 0
    assert "skipped" in result.stdout
    assert "surviving transcript" in result.stdout


def test_our_cost_unavailable_skips(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_cost_signals(monkeypatch, ours=None)
    result = runner.invoke(app, ["doctor", "--cost-check"])
    assert result.exit_code == 0
    assert "skipped" in result.stdout


# --- AC5-FR: reference tool errors mid-run ----------------------------------------


def test_ac5_fr_ref_nonzero_exit_skips_not_warns(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_cost_signals(monkeypatch, ref=(None, "reference cost tool exited 1"))
    result = runner.invoke(app, ["doctor", "--cost-check"])
    assert result.exit_code == 0
    assert "skipped (reference cost tool exited 1)" in result.stdout
    assert "WARN" not in result.stdout


def test_ac5_fr_unparseable_ref_output_skips(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _stub_cost_signals(
        monkeypatch, ref=(None, "reference cost tool emitted unparseable output")
    )
    result = runner.invoke(app, ["doctor", "--cost-check"])
    assert result.exit_code == 0
    assert "skipped" in result.stdout


# --- default doctor run unaffected --------------------------------------------------


def test_default_doctor_run_never_touches_cost_collectors(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def _boom() -> None:
        raise AssertionError("cost collector invoked without --cost-check")

    monkeypatch.setattr(doctor, "_find_recent_session_with_transcript", _boom)
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: Path("/src"))
    monkeypatch.setattr(doctor, "_source_rev", lambda source: "abc123")
    monkeypatch.setattr(doctor, "_read_marker", lambda: "abc123")
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: "present")
    monkeypatch.setattr(
        doctor, "_rust_report", lambda: {"binary": None, "revision": None}
    )
    monkeypatch.setattr(doctor, "_read_rust_marker", lambda: None)
    monkeypatch.setattr(doctor, "_rust_source_rev", lambda source: None)
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: False)
    # A default doctor run reaches the agent-health collectors, and a dead
    # sh.fno.* agent legitimately exits 1 - so leaving these live would make this
    # assertion depend on the developer's own launchctl.
    monkeypatch.setattr(
        doctor, "_launch_agent_failures", lambda: {"applicable": True, "dead": []}
    )
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "ran", "hours": 1.0, "stale": False, "agent_installed": True},
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "cost-check" not in result.stdout


# --- reference tool payload parsing (shape liberality) -------------------------------


@pytest.mark.parametrize(
    "payload",
    [
        {"sessions": [{"sessionId": SID, "totalCost": 7.5}]},
        [{"sessionId": SID, "totalCost": 7.5}],
        {"sessions": [{"session_id": SID, "cost_usd": 7.5}]},
        # Project-qualified session key (suffix match on the UUID)
        {"sessions": [{"sessionId": f"-Users-x-proj/{SID}", "totalCost": 7.5}]},
    ],
)
def test_ref_payload_shapes(monkeypatch: pytest.MonkeyPatch, payload) -> None:
    import json as _json
    import subprocess as _subprocess

    monkeypatch.setattr(
        doctor.shutil, "which", lambda name: f"/usr/local/bin/{name}"
    )

    def _fake_run(*args, **kwargs):
        class R:
            returncode = 0
            stdout = _json.dumps(payload)
            stderr = ""

        return R()

    monkeypatch.setattr(_subprocess, "run", _fake_run)
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run)
    cost, reason = doctor._run_ref_cost_tool(SID)
    assert reason is None
    assert cost == 7.5


def test_ref_session_missing_from_payload(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    import json as _json

    monkeypatch.setattr(
        doctor.shutil, "which", lambda name: f"/usr/local/bin/{name}"
    )

    def _fake_run(*args, **kwargs):
        class R:
            returncode = 0
            stdout = _json.dumps({"sessions": [{"sessionId": "other", "totalCost": 1}]})
            stderr = ""

        return R()

    monkeypatch.setattr(doctor.subprocess, "run", _fake_run)
    cost, reason = doctor._run_ref_cost_tool(SID)
    assert cost is None
    assert "not present" in reason
