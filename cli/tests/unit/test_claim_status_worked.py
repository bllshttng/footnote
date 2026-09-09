"""Machine-readable live-worker evidence for claim status."""
from __future__ import annotations

import json

from typer.testing import CliRunner

from fno.claims.cli import RosterReading, cli


runner = CliRunner()


def test_ac6_edge_claim_status_names_live_worker(monkeypatch):
    row = {
        "name": "bp-worker",
        "state": "working",
        "cwd": "/worktrees/ac1-node",
        "row_id": "session-1",
    }
    reading = RosterReading(True, 1, {"ac1-node": [row]}, "", {"session-1": row}, 0, ())
    monkeypatch.setattr("fno.claims.cli.read_roster", lambda **_kw: reading)
    monkeypatch.setattr(
        "fno.claims.cli._claims_core.claim_status",
        lambda **_kw: {"key": "node:ac1-node", "state": "free"},
    )

    result = runner.invoke(cli, ["status", "node:ac1-node", "--json"])

    assert result.exit_code == 0, result.output
    info = json.loads(result.stdout)
    assert info["state"] == "free"
    assert info["worked_by"] == ["bp-worker"]
    assert info["basis"] == "live-worker"
