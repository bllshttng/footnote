"""Codex pane receipts require an addressable session binding."""
from __future__ import annotations

import os

from typer.testing import CliRunner

import fno.agents.cli as agents_cli
import fno.agents.mux_spawn as mux_spawn
from tests.agents.test_spawn_pane import (
    FakeRunner,
    stub_codex_sandbox_probe,
    use_tmpdir,
)


def test_cmd_spawn_pane_refuses_unbound_codex_receipt(
    tmp_path, monkeypatch, loop_admission_ready
) -> None:
    """The public CLI never exits zero with an unaddressable Codex pane."""
    use_tmpdir(monkeypatch, tmp_path)
    stub_codex_sandbox_probe(monkeypatch)
    fake_runner = FakeRunner(run_stdout="9\n")
    real_dispatch = mux_spawn.dispatch_spawn_pane

    def dispatch_with_fake_mux(**kwargs):
        kwargs.pop("workspace", None)
        kwargs.pop("bounded_placement", None)
        return real_dispatch(**kwargs, runner=fake_runner)

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_bounded_pane", dispatch_with_fake_mux)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_SESSION", "main")

    from fno.agents import events as _events

    emitted: list = []
    monkeypatch.setattr(
        _events, "emit", lambda name_, **kw: emitted.append((name_, kw))
    )
    # Pin caller and canonical roots together so the receipt path is stable.
    monkeypatch.setenv("FNO_REPO_ROOT", os.getcwd())

    result = CliRunner().invoke(
        agents_cli.agents_app,
        [
            "spawn",
            "--name",
            "peer",
            "--harness",
            "codex",
            "--substrate",
            "pane",
            "/fno:target scratch",
        ],
    )
    assert result.exit_code == 1, result.output
    assert "required codex session binding" in result.output
    assert "fno doctor --codex-bind" not in result.output
    assert fake_runner.kill_calls
    uncaptured = [
        kwargs
        for name, kwargs in emitted
        if name == "agent_session_id_uncaptured" and kwargs.get("harness") == "codex"
    ]
    assert uncaptured, "the unbound codex spawn emitted no uncaptured event"
    assert set(uncaptured[-1]) >= {"elapsed_s", "window_s", "polls", "condition"}
