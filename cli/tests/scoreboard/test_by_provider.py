"""Kept cli-level coverage for `fno scoreboard --by-provider` (x-140c, x-59e1).

The fold and renderer live in crates/fno-agents/src/scoreboard_provider.rs
now; their contracts moved to the Rust tests there. What stays here is what
exercises cli.py itself: the -J JSON contract, the flag guard, and the
corrupt-ledger exit.
"""

from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
from datetime import datetime, timedelta

import typer
from typer.testing import CliRunner

from fno.scoreboard import cli as sb_cli

runner = CliRunner()
NOW = datetime(2026, 7, 3, 20, 0, 0)


def _row(provider=None, model=None, tr="DonePRGreen", nid=None, cost=1.0, completed="2026-07-03T10:00:00", **extra):
    r = {"type": "execution", "completed": completed, "termination_reason": tr, "cost_usd": cost}
    if provider:
        r["provider_id"] = provider
    if model:
        r["model"] = model
    if nid:
        r["graph_node_id"] = nid
    r.update(extra)
    return r


def _ledger(tmp_path, rows):
    p = tmp_path / "ledger.json"
    p.write_text(json.dumps({"entries": rows}))
    return p


def _app():
    app = typer.Typer()
    app.command()(sb_cli.scoreboard_command)
    return app


def _wire(monkeypatch, tmp_path, ledger_path):
    import fno.paths as paths

    monkeypatch.setattr(paths, "ledger_json", lambda: ledger_path)
    monkeypatch.setattr(paths, "graph_json", lambda: tmp_path / "graph.json")


# Graph with W4 causal telemetry so shipped nodes are judgeable. The nodes
# are merged: a delivered terminal proves a run shipped only when the node
# actually delivered, so a fixture that means "shipped" must say merged.
GRAPH = [
    {"id": "x-1", "reverted": False, "merge_status": "merged", "completed_at": "2026-07-03T10:00:00"},
    {"id": "x-2", "reverted": False, "merge_status": "merged", "completed_at": "2026-07-03T10:00:00"},
]


# --- AC1-HP -------------------------------------------------------------------
def test_hp_json_contract(tmp_path, monkeypatch):
    ts = (datetime.now() - timedelta(days=1)).isoformat()
    rows = [_row("claude", "opus", nid="x-1", cost=6.0, completed=ts)]
    seed_graph(tmp_path / "graph.json", json.dumps({"entries": GRAPH}))
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), ["--by-provider", "-J"])
    assert res.exit_code == 0, res.output
    pb = json.loads(res.output)
    assert pb["state"] == "ok" and pb["since_days"] == 28
    assert set(pb["coverage"]) == {
        "rows", "harness_pct", "provider_pct", "model_pct", "attributed_pct"
    }
    row = pb["rows"][0]
    # every rate rides with its denominator in the same object
    assert {"provider", "model", "runs", "shipped", "spend_usd", "cost_per_shipped_usd",
            "bounce_rate_pct", "shipped_linked", "median_iterations", "retry_rows"} <= set(row)



def test_view_flags_mutually_exclusive(tmp_path, monkeypatch):
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, []))
    res = runner.invoke(_app(), ["--by-provider", "--by-skill"])
    assert res.exit_code != 0
    assert "mutually exclusive" in res.output


# --- AC3-ERR ------------------------------------------------------------------

def test_fr_corrupt_ledger_exit_1(tmp_path, monkeypatch):
    p = tmp_path / "ledger.json"
    p.write_text('{"entries": [ {corrupt')
    _wire(monkeypatch, tmp_path, p)
    res = runner.invoke(_app(), ["--by-provider"])
    assert res.exit_code == 1
    assert "ledger.json" in res.output and "byte" in res.output
