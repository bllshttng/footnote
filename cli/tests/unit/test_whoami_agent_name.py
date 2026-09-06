"""Tests for the `agent:` line's registry fallback on `fno whoami` (x-8bfb).

x-301a already prints `agent:    {FNO_AGENT_SELF} (mesh)` when the env var is
set. That line fires only for a process the spawn path started with the
variable set. This fallback covers the gap: a worker whose env lacks it (an
adopted session, a restored pane, a `/fno-me` join) still learns its own name
from the registry row that already resolves it, via `resolve_self` tier 2
(session-fallback) with no `claude` subprocess.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import context_probe
from fno.agents.registry import AgentEntry
from fno.cli import app
from fno.harness_identity import OwnedHarnessIdentity

FIXTURES = Path(__file__).parent / "fixtures" / "agent"


def _workspace(tmp_path: Path) -> Path:
    project = tmp_path / "project"
    (project / ".fno").mkdir(parents=True)
    (project / ".fno" / "target-state.md").write_text(
        (FIXTURES / "target-state.md").read_text()
    )
    return project


def _entry(**kw) -> AgentEntry:
    base: dict = {"name": "x", "harness": "claude", "cwd": "/w", "log_path": ""}
    base.update(kw)
    return AgentEntry(**base)


@pytest.fixture(autouse=True)
def _quiet_context(monkeypatch, tmp_path):
    # Irrelevant to this line; keep the summary's other enrichments silent so
    # assertions are not sensitive to ambient machine state.
    monkeypatch.setattr(context_probe, "probe_context", lambda transcript_path=None: None)
    for var in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)


def test_agent_line_env_tier_unchanged(monkeypatch, tmp_path):
    # AC2-HP: FNO_AGENT_SELF set -> the env tier renders byte-for-byte what it
    # printed before this change, and the registry is never consulted.
    project = _workspace(tmp_path)
    monkeypatch.chdir(project)
    monkeypatch.setenv("FNO_AGENT_SELF", "t-env-worker")

    def _boom(*a, **kw):
        raise AssertionError("registry fallback must not run when FNO_AGENT_SELF is set")

    monkeypatch.setattr("fno.agents.registry.load_registry", _boom)
    result = CliRunner().invoke(app, ["whoami"])
    assert result.exit_code == 0, result.stdout + result.stderr
    assert "agent:    t-env-worker (mesh)" in result.stdout
    assert "(registry)" not in result.stdout


def test_agent_line_registry_fallback_when_env_unset(monkeypatch, tmp_path):
    # AC1-HP: FNO_AGENT_SELF unset but the harness session id matches a
    # registry row -> the agent: line names that row, with no subprocess.
    project = _workspace(tmp_path)
    monkeypatch.chdir(project)

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda env=None: OwnedHarnessIdentity(
            session_id="sess-123", harness="claude", disposition="single"
        ),
    )
    row = _entry(name="t-adopted-worker", harness_session_id="sess-123")
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])

    def _boom(*a, **kw):
        raise AssertionError("AC4-FR: no claude subprocess on this path")

    monkeypatch.setattr("fno.agents.harnesses.claude._subprocess_run", _boom)

    result = CliRunner().invoke(app, ["whoami"])
    assert result.exit_code == 0, result.stdout + result.stderr
    assert "agent:    t-adopted-worker (registry)" in result.stdout


def test_agent_line_absent_for_human_session(monkeypatch, tmp_path):
    # AC3-HP: no env, no matching registry row -> byte-for-byte unchanged,
    # no agent: line, exit 0.
    project = _workspace(tmp_path)
    monkeypatch.chdir(project)
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda env=None: OwnedHarnessIdentity(session_id=None, harness=None, disposition="empty"),
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])

    result = CliRunner().invoke(app, ["whoami"])
    assert result.exit_code == 0, result.stdout + result.stderr
    assert "agent:" not in result.stdout
