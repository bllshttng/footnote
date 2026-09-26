"""Node-seeded pane spawns carry their provenance to the worker."""
from __future__ import annotations

from pathlib import Path

from typer.testing import CliRunner

import fno.agents.cli as agents_cli
import fno.agents.mux_spawn as mux_spawn
from fno.agents.mux_spawn import MuxSpawnResult


def test_cmd_spawn_node_flag_resolves_and_passes_provenance(
    tmp_path: Path, monkeypatch, loop_admission_ready
) -> None:
    node_id = "x-a"
    captured: dict = {}

    def fake_dispatch(**kwargs):
        captured.update(kwargs)
        return MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="main",
            pane_id=1,
            child_pid=None,
            session_uuid="u",
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_bounded_pane", fake_dispatch)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [
            {
                "id": node_id,
                "slug": "s",
                "dispatch_verb": "/target",
                "difficulty": "low",
            }
        ],
    )

    result = CliRunner().invoke(
        agents_cli.agents_app,
        [
            "spawn",
            "peer",
            "--harness",
            "claude",
            "--substrate",
            "pane",
            "--node",
            node_id,
            "--slug",
            "s",
            "--plan",
            "p.md",
            "--session-phase",
            "do",
        ],
    )
    assert result.exit_code == 0, result.output
    assert captured["provenance"] == {
        "FNO_NODE": node_id,
        "FNO_SLUG": "s",
        "FNO_PLAN": "p.md",
        "FNO_NODE_CLAIM_HOLDER": "spawn-handover:t-a-s",
    }
