from __future__ import annotations

from datetime import datetime

import typer
from typer.testing import CliRunner

from fno.scoreboard.fold import build_lanes

NOW = datetime(2026, 8, 20, 12, 0, 0)
RUN = {
    "type": "execution",
    "provider": "zai",
    "model": "zai-coding-plan/glm-5.3",
    "effort": "low",
    "graph_node_id": "x-1",
    "completed": "2026-08-20T10:00:00",
    "duration_minutes": 12.0,
    "termination_reason": "DonePRGreen",
    "carveouts_filed": 0,
}


def test_build_lanes_has_coverage_retrospective_and_live_sections():
    result = build_lanes(
        [RUN],
        [{"id": "x-1", "size": "S"}],
        [
            {
                "provider": "zai",
                "model": "zai-coding-plan/glm-5.3",
                "effort": "low",
                "status": "live",
            }
        ],
        [{"kind": "provider_rate_limited", "provider": "zai", "ts": "2026-08-20T11:00:00Z"}],
        {"zai": 5},
        since_days=28,
        now=NOW,
    )

    assert result["coverage"] == {"provider": 1, "model": 1, "effort": 1, "rows": 1}
    row = result["retrospective"][0]
    assert (row["provider"], row["model"], row["effort"], row["size"]) == (
        "zai",
        "zai-coding-plan/glm-5.3",
        "low",
        "S",
    )
    assert row["runs"] == 1
    assert row["sample_state"] == "insufficient_sample"
    assert result["live"][0]["occupancy"] == 1
    assert result["live"][0]["cap"] == 5
    assert result["rate_limited"] == 1


def test_lanes_cli_renders_coverage(tmp_path, monkeypatch):
    from fno.scoreboard import cli as sb_cli

    monkeypatch.setattr(sb_cli._paths, "ledger_json", lambda: tmp_path / "ledger.json")
    monkeypatch.setattr(sb_cli._paths, "graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(sb_cli._paths, "agents_registry_path", lambda: tmp_path / "registry.json")
    (tmp_path / "ledger.json").write_text('{"entries": []}')
    (tmp_path / "graph.json").write_text('{"entries": []}')
    (tmp_path / "registry.json").write_text('{"schema_version": 15, "agents": []}')

    app = typer.Typer()
    app.command()(sb_cli.scoreboard_command)
    result = CliRunner().invoke(app, ["--lanes"])
    assert result.exit_code == 0, result.output
    assert "Coverage" in result.output


def test_lanes_cli_reads_provider_limits_after_the_rename(tmp_path, monkeypatch):
    # The lanes arm reads the cap table off settings.agents; after the
    # max_lanes -> provider_limits rename an unchanged getattr silently
    # passed {} and live occupancy rendered cap-less while the gate still
    # enforced one. A settings object exposing ONLY the new field must reach
    # build_lanes with the configured caps.
    from types import SimpleNamespace

    from fno.scoreboard import cli as sb_cli

    captured: dict = {}

    def fake_build_lanes(*args, **kwargs):
        captured["caps"] = args[4]
        return {"coverage": {"provider": 0, "model": 0, "effort": 0, "rows": 0}}

    monkeypatch.setattr(sb_cli, "build_lanes", fake_build_lanes)
    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda: SimpleNamespace(
            agents=SimpleNamespace(provider_limits={"zai": {"lanes": 5, "subagents": 1}})
        ),
    )
    monkeypatch.setattr(sb_cli._paths, "ledger_json", lambda: tmp_path / "ledger.json")
    monkeypatch.setattr(sb_cli._paths, "graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(sb_cli._paths, "agents_registry_path", lambda: tmp_path / "registry.json")
    (tmp_path / "ledger.json").write_text('{"entries": []}')
    (tmp_path / "graph.json").write_text('{"entries": []}')
    (tmp_path / "registry.json").write_text('{"schema_version": 15, "agents": []}')

    app = typer.Typer()
    app.command()(sb_cli.scoreboard_command)
    result = CliRunner().invoke(app, ["--lanes", "--json"])
    assert result.exit_code == 0, result.output
    assert captured["caps"] == {"zai": {"lanes": 5, "subagents": 1}}
