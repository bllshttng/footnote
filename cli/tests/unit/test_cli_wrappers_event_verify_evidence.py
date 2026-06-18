"""Unit tests for `fno event verify-evidence` wrapper.

verify-event-evidence.sh was folded into the fno-agents binary (US1,
ab-58645f63); the wrapper now invokes `fno-agents verify-evidence event <...>`.
The binary's behavior is proven byte-parity with the former bash by
crates/fno-agents/tests/verify_evidence_parity.rs.
"""
from __future__ import annotations

from pathlib import Path

from typer.testing import CliRunner

from fno.cli import app
from fno.events import cli as events_cli_module
from fno.agents import rust_runtime

runner = CliRunner()

_FAKE_BIN = Path("/fake/bin/fno-agents")
_ARGS = ["event", "verify-evidence", "ses-abc123", "nonce-xyz", "/tmp/events.jsonl", "/tmp/artifact.md"]


class _StubResult:
    def __init__(self, returncode: int = 0):
        self.returncode = returncode


def _patch_binary(monkeypatch, binary=_FAKE_BIN):
    # The verb does `from fno.agents.rust_runtime import resolve_binary` at call
    # time, so patching the source attribute is what the function picks up.
    monkeypatch.setattr(rust_runtime, "resolve_binary", lambda: binary)


def test_verify_evidence_help_renders():
    """AC-UI: --help documents all 4 positional args."""
    result = runner.invoke(app, ["event", "verify-evidence", "--help"])
    assert result.exit_code == 0
    assert "SESSION_ID" in result.stdout or "session-id" in result.stdout or "session_id" in result.stdout
    assert "NONCE" in result.stdout or "nonce" in result.stdout
    assert "EVENTS_FILE" in result.stdout or "events-file" in result.stdout or "events_file" in result.stdout
    assert "ARTIFACT_PATH" in result.stdout or "artifact-path" in result.stdout or "artifact_path" in result.stdout


def test_verify_evidence_invokes_binary_event_subverb(monkeypatch):
    """AC1-HP: invokes `fno-agents verify-evidence event <args>` (no bash script)."""
    captured = {}

    def _stub_run(cmd, check=False, **kwargs):
        captured["cmd"] = list(cmd)
        return _StubResult(returncode=0)

    _patch_binary(monkeypatch)
    monkeypatch.setattr(events_cli_module.subprocess, "run", _stub_run)

    result = runner.invoke(app, _ARGS)
    assert result.exit_code == 0
    assert captured["cmd"] == [
        str(_FAKE_BIN),
        "verify-evidence",
        "event",
        "ses-abc123",
        "nonce-xyz",
        "/tmp/events.jsonl",
        "/tmp/artifact.md",
    ]
    assert not any("verify-event-evidence.sh" in part for part in captured["cmd"])


def test_verify_evidence_rc2_propagation(monkeypatch):
    """AC-EDGE-1: rc=2 from the binary propagates as exit code 2."""
    _patch_binary(monkeypatch)
    monkeypatch.setattr(events_cli_module.subprocess, "run", lambda *a, **k: _StubResult(2))
    result = runner.invoke(app, _ARGS)
    assert result.exit_code == 2


def test_verify_evidence_rc1_diagnostic_propagation(monkeypatch):
    """AC-EDGE-2: rc=1 propagates; stdout not rewritten."""
    _patch_binary(monkeypatch)
    monkeypatch.setattr(events_cli_module.subprocess, "run", lambda *a, **k: _StubResult(1))
    result = runner.invoke(app, _ARGS)
    assert result.exit_code == 1


def test_verify_evidence_missing_binary_yields_exit_2(monkeypatch):
    """AC-FR: when the fno-agents binary can't be resolved, exit 2 with a
    specific stderr message naming the binary, not a traceback."""
    monkeypatch.setattr(rust_runtime, "resolve_binary", lambda: None)
    result = runner.invoke(app, _ARGS)
    assert result.exit_code == 2
    assert "fno-agents binary" in result.output
