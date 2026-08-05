"""Tests for the `context:` line on `fno whoami` (x-7685, US1/AC2/AC3).

The line is an enrichment of the confused-agent recovery verb, so it follows
the rule every other enrichment in whoami follows: it must never add a failure
mode. A None reading (fresh session, no assistant usage row, unreadable store)
OMITS the line entirely, keeping a fresh SessionStart byte-for-byte unchanged
while a post-compaction resume prints a real number exactly when re-orientation
matters.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import context_probe
from fno.cli import app

FIXTURES = Path(__file__).parent / "fixtures" / "agent"


def _workspace(tmp_path: Path) -> Path:
    project = tmp_path / "project"
    (project / ".fno").mkdir(parents=True)
    (project / ".fno" / "target-state.md").write_text(
        (FIXTURES / "target-state.md").read_text()
    )
    return project


@pytest.fixture(autouse=True)
def _no_ambient_markers(monkeypatch):
    # whoami probes the live transcript only via probe_context; clear ambient
    # identity so the real probe floors to None unless the test patches it.
    for var in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)


def test_context_text_line_when_reading(monkeypatch, tmp_path):
    project = _workspace(tmp_path)
    monkeypatch.chdir(project)
    monkeypatch.setattr(
        context_probe,
        "probe_context",
        lambda transcript_path=None: context_probe.ContextReading(
            used_tokens=307850, window_tokens=1000000, used_pct=31, model="claude-opus-5"
        ),
    )
    result = CliRunner().invoke(app, ["whoami"])
    assert result.exit_code == 0, result.stdout + result.stderr
    assert "context:  31% used (307,850 of 1,000,000 tokens)" in result.stdout


def test_context_line_omitted_when_none(monkeypatch, tmp_path):
    # A None reading omits the line entirely - whoami gains no failure mode and
    # a fresh SessionStart stays byte-identical (AC3).
    project = _workspace(tmp_path)
    monkeypatch.chdir(project)
    monkeypatch.setattr(context_probe, "probe_context", lambda transcript_path=None: None)
    result = CliRunner().invoke(app, ["whoami"])
    assert result.exit_code == 0, result.stdout + result.stderr
    assert "context:" not in result.stdout


def test_context_json_fields_when_reading(monkeypatch, tmp_path):
    project = _workspace(tmp_path)
    monkeypatch.chdir(project)
    monkeypatch.setattr(
        context_probe,
        "probe_context",
        lambda transcript_path=None: context_probe.ContextReading(
            used_tokens=307850, window_tokens=1000000, used_pct=31, model="claude-opus-5"
        ),
    )
    result = CliRunner().invoke(app, ["whoami", "--json"])
    assert result.exit_code == 0, result.stdout + result.stderr
    payload = json.loads(result.stdout)
    assert payload["context_used_pct"] == 31
    assert payload["context_used_tokens"] == 307850
    assert payload["context_window_tokens"] == 1000000


def test_context_json_fields_absent_when_none(monkeypatch, tmp_path):
    project = _workspace(tmp_path)
    monkeypatch.chdir(project)
    monkeypatch.setattr(context_probe, "probe_context", lambda transcript_path=None: None)
    result = CliRunner().invoke(app, ["whoami", "--json"])
    assert result.exit_code == 0, result.stdout + result.stderr
    payload = json.loads(result.stdout)
    assert "context_used_pct" not in payload
    assert "context_used_tokens" not in payload
