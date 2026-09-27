"""Codex successor spawns use the bounded pane route."""
from __future__ import annotations

from types import SimpleNamespace

from typer.testing import CliRunner

import fno.adapters.providers.dispatch as provider_dispatch
import fno.adapters.providers.loader as provider_loader
import fno.agents.cli as agents_cli
import fno.agents.mux_spawn as mux_spawn
from fno.agents.mux_spawn import MuxSpawnResult
from tests.agents._fake_claude import stub_codex_sandbox_probe


def test_codex_successor_uses_bounded_dispatch_without_claude_route(
    tmp_path, monkeypatch, loop_admission_ready
) -> None:
    stub_codex_sandbox_probe(monkeypatch)
    captured = {}

    def fake_bounded(**kwargs):
        captured.update(kwargs)
        return MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="main",
            pane_id=1,
            child_pid=None,
            session_uuid="codex-thread",
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_bounded_pane", fake_bounded)
    monkeypatch.setattr(
        provider_loader,
        "load_providers",
        lambda **_kwargs: SimpleNamespace(
            by_id={"work": SimpleNamespace(harness="codex")}
        ),
    )
    monkeypatch.setattr(
        provider_dispatch,
        "dispatch_env",
        lambda *_args, **_kwargs: {"CODEX_HOME": "/tmp/codex"},
    )
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    result = CliRunner().invoke(
        agents_cli.agents_app,
        [
            "spawn",
            "--name",
            "successor",
            "--harness",
            "codex",
            "--substrate",
            "pane",
            "--bounded-placement",
            "--recorded-provider=openai",
            "--model",
            "gpt-5.6-sol",
            "--dispatch-account",
            "work",
            "/fno:target --no-merge scratch",
        ],
    )

    assert result.exit_code == 0, result.output
    assert captured["provider"] == "codex"
    assert captured["route_provider_id"] == "openai"
    assert captured["account_record_id"] == "work"
    assert captured["model_name"] == "gpt-5.6-sol"
    assert captured["workspace"] is None
    assert captured["tab"] is None
