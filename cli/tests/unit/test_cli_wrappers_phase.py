"""Unit tests for `fno phase kill-check` wrapper.

kill-criteria.sh was folded into the fno-agents binary (US1, ab-58645f63); the
wrapper now resolves the binary via fno.agents.rust_runtime.resolve_binary and
invokes `fno-agents kill-check <plan_path>`. The binary's own behavior is proven
byte-parity with the former bash by crates/fno-agents/tests/kill_criteria_parity.rs.

Note: fno phase verify tests removed in Task 3.2 (control-plane collapse,
ab-d0337fbc) -- phase-verifier.sh deleted, fno phase verify subcommand removed.
"""
from __future__ import annotations

from pathlib import Path

from typer.testing import CliRunner

from fno.cli import app
from fno import phase as phase_module

runner = CliRunner()

_FAKE_BIN = Path("/fake/bin/fno-agents")


class _StubResult:
    def __init__(self, returncode: int = 0):
        self.returncode = returncode


def _patch_binary(monkeypatch, binary=_FAKE_BIN):
    monkeypatch.setattr(phase_module.cli, "resolve_binary", lambda: binary)


def test_kill_check_help_renders():
    result = runner.invoke(app, ["phase", "kill-check", "--help"])
    assert result.exit_code == 0
    assert "kill" in result.stdout.lower() or "plan" in result.stdout.lower()


def test_kill_check_missing_binary_yields_exit_2(monkeypatch):
    """AC-FR: when the fno-agents binary can't be resolved, rc=2 with a useful
    message (names the binary + how to install it), not a traceback."""
    monkeypatch.setattr(phase_module.cli, "resolve_binary", lambda: None)
    result = runner.invoke(app, ["phase", "kill-check", "/some/plan"])
    assert result.exit_code == 2
    assert "fno-agents binary" in result.output


def test_kill_check_invokes_binary_with_plan_path(monkeypatch):
    """AC1-HP: invokes `fno-agents kill-check <plan_path>` (no kill-criteria.sh);
    exit code propagates."""
    captured = {}

    def _stub_run(cmd, check=False, **kwargs):
        captured["cmd"] = list(cmd)
        return _StubResult(returncode=5)

    _patch_binary(monkeypatch)
    monkeypatch.setattr(phase_module.cli.subprocess, "run", _stub_run)

    result = runner.invoke(app, ["phase", "kill-check", "/some/plan/path"])
    assert result.exit_code == 5

    cmd = captured["cmd"]
    assert cmd == [str(_FAKE_BIN), "kill-check", "/some/plan/path"]
    # No reference to the deleted bash script anywhere in the invocation.
    assert not any("kill-criteria.sh" in part for part in cmd)


def test_kill_check_no_kill_returns_zero(monkeypatch):
    """AC-EDGE-1: rc=0 (no predicate fired) propagates."""
    _patch_binary(monkeypatch)
    monkeypatch.setattr(
        phase_module.cli.subprocess, "run", lambda *a, **k: _StubResult(returncode=0)
    )
    result = runner.invoke(app, ["phase", "kill-check", "/some/plan"])
    assert result.exit_code == 0


def test_kill_check_predicate_fired_returns_one(monkeypatch):
    """AC-EDGE-1: rc=1 (predicate fired) propagates."""
    _patch_binary(monkeypatch)
    monkeypatch.setattr(
        phase_module.cli.subprocess, "run", lambda *a, **k: _StubResult(returncode=1)
    )
    result = runner.invoke(app, ["phase", "kill-check", "/some/plan"])
    assert result.exit_code == 1


def test_kill_check_uses_state_plan_path_when_omitted(tmp_path, monkeypatch):
    """AC-HP: when PLAN_PATH is not given, the wrapper reads plan_path from
    .fno/target-state.md and forwards it to the binary."""
    state_dir = tmp_path / ".fno"
    state_dir.mkdir()
    state_file = state_dir / "target-state.md"
    state_file.write_text(
        "---\nplan_path: /state/derived/plan\n---\nbody\n",
        encoding="utf-8",
    )

    captured = {}

    def _stub_run(cmd, check=False, **kwargs):
        captured["cmd"] = list(cmd)
        return _StubResult(returncode=0)

    _patch_binary(monkeypatch)
    monkeypatch.setattr(phase_module.cli.subprocess, "run", _stub_run)
    monkeypatch.setattr(phase_module.cli, "_state_file_path", lambda: state_file)

    result = runner.invoke(app, ["phase", "kill-check"])
    assert result.exit_code == 0
    assert captured["cmd"] == [str(_FAKE_BIN), "kill-check", "/state/derived/plan"]
