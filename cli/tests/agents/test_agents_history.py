"""Owner-boundary tests for the native history forwarder."""

from __future__ import annotations

import os
from pathlib import Path
from types import SimpleNamespace

import typer
from typer.testing import CliRunner

import fno.agents.history as history_module
from fno.agents.history import history_command


def _install_forwarder(monkeypatch, calls, slug):
    configured = {
        "graph_json": Path("/configured/graph.db"),
        "ledger_json": Path("/configured/ledger.json"),
        "global_events_json": Path("/configured/events.jsonl"),
        "agents_home_dir": Path("/configured/agents"),
    }
    paths = SimpleNamespace(
        **{name: (lambda path=path: path) for name, path in configured.items()}
    )
    monkeypatch.setattr(history_module, "_paths", paths)
    monkeypatch.setattr(
        history_module,
        "resolve_current_repo_slug",
        lambda cwd: slug,
        raising=False,
    )
    monkeypatch.setattr(
        history_module,
        "resolve_native_bin",
        lambda: "/native/fno",
        raising=False,
    )
    monkeypatch.setattr(os, "execv", lambda binary, argv: calls.append((binary, argv)))

    return configured


def _expected_argv(configured, slug):
    argv = [
        "fno",
        "agents",
        "history",
        "x-3344",
        "--graph",
        str(configured["graph_json"]),
        "--ledger",
        str(configured["ledger_json"]),
        "--events",
        str(configured["global_events_json"]),
        "--agents-home",
        str(configured["agents_home_dir"]),
    ]
    if slug:
        argv.extend(["--repo-slug", slug])
    return argv


def test_forwarder_execs_resolved_paths_with_optional_repo_slug(monkeypatch):
    calls = []
    for slug in ("o/r", None):
        configured = _install_forwarder(monkeypatch, calls, slug)
        try:
            history_command("x-3344")
        except typer.Exit:
            pass
        assert calls and calls[-1] == ("/native/fno", _expected_argv(configured, slug))


def test_whoami_ledger_alias_is_hidden_and_forwards_to_history(monkeypatch):
    from fno.agent.cli import whoami_app

    calls = []
    configured = _install_forwarder(monkeypatch, calls, "o/r")
    runner = CliRunner()
    assert "ledger" not in runner.invoke(whoami_app, ["--help"]).output

    result = runner.invoke(whoami_app, ["ledger", "x-3344"])
    assert calls == [("/native/fno", _expected_argv(configured, "o/r"))]
    assert result.exit_code == 0


def test_agents_help_advertises_history():
    from fno.agents.cli import agents_app

    result = CliRunner().invoke(agents_app, ["--help"])
    assert "history" in result.output
