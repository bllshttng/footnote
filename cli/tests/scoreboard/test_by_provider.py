"""AC coverage for `fno scoreboard --by-provider` (x-140c).

AC1-HP   per-provider rows with runs/shipped/spend/cost-per-shipped.
AC2-HP   -J JSON contract for quota-aware dispatch (x-5d3e).
AC3-ERR  missing/corrupt graph -> bounce_rate_pct null, never 0%.
AC4-UI   no-data line; unattributed bucket + attributed_pct coverage.
AC5-EDGE zero-shipped provider -> spend shown, cost_per_shipped null.
AC6-FR   by-provider path inherits the ledger reader's retry/BrokenLedger.
"""

from __future__ import annotations

import json
from datetime import datetime, timedelta

import typer
from typer.testing import CliRunner

from fno.scoreboard import cli as sb_cli
from fno.scoreboard.fold import build_provider_scoreboard

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


# Graph with W4 causal telemetry so shipped nodes are judgeable.
GRAPH = [{"id": "x-1", "reverted": False}, {"id": "x-2", "reverted": False}]


# --- AC1-HP -------------------------------------------------------------------
def test_hp_grouping_and_cost_per_shipped():
    rows = [
        _row("claude", "opus", nid="x-1", cost=6.0),
        _row("claude", "opus", tr="NoProgress", cost=4.0),
        _row("codex", "gpt-5", nid="x-2", cost=3.0),
    ]
    pb = build_provider_scoreboard(rows, GRAPH, since_days=28, now=NOW)
    assert pb["state"] == "ok"
    by = {(r["provider"], r["model"]): r for r in pb["rows"]}
    claude = by[("claude", "opus")]
    # cost-per-shipped = ALL window spend (wedge included) / shipped count
    assert claude["runs"] == 2 and claude["shipped"] == 1
    assert claude["spend_usd"] == 10.0 and claude["cost_per_shipped_usd"] == 10.0
    codex = by[("codex", "gpt-5")]
    assert codex["runs"] == 1 and codex["cost_per_shipped_usd"] == 3.0


def test_hp_non_execution_rows_excluded():
    rows = [
        _row("claude", "opus", nid="x-1", cost=5.0),
        {"type": "think", "completed": "2026-07-03T10:00:00", "cost_usd": 9.0, "provider_id": "claude"},
        {"completed": "2026-07-03T10:00:00", "cost_usd": 9.0},  # backfill row, no type
    ]
    pb = build_provider_scoreboard(rows, GRAPH, since_days=28, now=NOW)
    assert pb["coverage"]["rows"] == 1
    assert pb["rows"][0]["spend_usd"] == 5.0


def test_hp_bounce_and_median_iterations():
    graph = GRAPH + [{"id": "x-9", "caused_by": "x-1", "created_at": "2026-07-04T00:00:00"}]
    rows = [
        _row("claude", "opus", nid="x-1", cost=2.0, iterations=3),
        _row("claude", "opus", nid="x-2", cost=2.0, iterations=7),
        _row("claude", "opus", tr="NoProgress", cost=1.0, iterations=99),  # wedged: excluded from median
    ]
    pb = build_provider_scoreboard(rows, graph, since_days=28, now=NOW)
    r = pb["rows"][0]
    assert r["shipped_linked"] == 2
    assert r["bounce_rate_pct"] == 50  # x-1 bounced (fix-node next day), x-2 clean
    assert r["median_iterations"] == 3  # over shipped rows only


def test_hp_retry_rows_counts_redispatches():
    rows = [
        _row("claude", "opus", nid="x-1", tr="NoProgress", cost=1.0),
        _row("claude", "opus", nid="x-1", cost=1.0),  # second row for the same node
        _row("claude", "opus", nid="x-2", cost=1.0),
    ]
    pb = build_provider_scoreboard(rows, GRAPH, since_days=28, now=NOW)
    assert pb["rows"][0]["retry_rows"] == 1


def test_hp_window_excludes_old_rows():
    rows = [
        _row("claude", "opus", nid="x-1", cost=5.0),
        _row("claude", "opus", cost=99.0, completed="2020-01-01T00:00:00"),
    ]
    pb = build_provider_scoreboard(rows, GRAPH, since_days=28, now=NOW)
    assert pb["coverage"]["rows"] == 1 and pb["rows"][0]["spend_usd"] == 5.0


# --- AC2-HP -------------------------------------------------------------------
def test_hp_json_contract(tmp_path, monkeypatch):
    ts = (datetime.now() - timedelta(days=1)).isoformat()
    rows = [_row("claude", "opus", nid="x-1", cost=6.0, completed=ts)]
    (tmp_path / "graph.json").write_text(json.dumps({"entries": GRAPH}))
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), ["--by-provider", "-J"])
    assert res.exit_code == 0, res.output
    pb = json.loads(res.output)
    assert pb["state"] == "ok" and pb["since_days"] == 28
    assert set(pb["coverage"]) == {"rows", "attributed_pct"}
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
def test_err_missing_graph_yields_null_bounce_not_zero(tmp_path, monkeypatch):
    ts = (datetime.now() - timedelta(days=1)).isoformat()
    rows = [_row("claude", "opus", nid="x-1", cost=2.0, completed=ts)]
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))  # no graph.json written
    res = runner.invoke(_app(), ["--by-provider", "-J"])
    assert res.exit_code == 0, res.output
    row = json.loads(res.output)["rows"][0]
    assert row["bounce_rate_pct"] is None and row["shipped_linked"] == 0


def test_err_graph_without_causal_telemetry_is_unjudgeable():
    # Node resolves but the graph carries no W4 fields: a fake 0% bounce would
    # be indistinguishable from "no revert data exists yet".
    rows = [_row("claude", "opus", nid="x-1", cost=2.0)]
    pb = build_provider_scoreboard(rows, [{"id": "x-1"}], since_days=28, now=NOW)
    r = pb["rows"][0]
    assert r["bounce_rate_pct"] is None and r["shipped_linked"] == 0


# --- AC4-UI -------------------------------------------------------------------
def test_ui_no_data_line(tmp_path, monkeypatch):
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, []))
    res = runner.invoke(_app(), ["--by-provider"])
    assert res.exit_code == 0
    assert "no terminal sessions in window" in res.output


def test_ui_unattributed_bucket_and_coverage(tmp_path, monkeypatch):
    ts = (datetime.now() - timedelta(days=1)).isoformat()
    rows = [
        _row("claude", "opus", nid="x-1", cost=2.0, completed=ts),
        _row(None, None, cost=3.0, completed=ts),  # pre-provider-stamp era row
    ]
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), ["--by-provider"])
    assert res.exit_code == 0, res.output
    assert "unattributed" in res.output
    assert "50%" in res.output  # attributed_pct on the coverage line


def test_ui_unattributed_never_dropped_spend_reconciles():
    rows = [
        _row("claude", "opus", nid="x-1", cost=2.0),
        _row(None, None, cost=3.0),
        _row("codex", None, tr="NoProgress", cost=5.0),
    ]
    pb = build_provider_scoreboard(rows, GRAPH, since_days=28, now=NOW)
    assert pb["coverage"] == {"rows": 3, "attributed_pct": 67}
    by = {r["provider"]: r for r in pb["rows"]}
    assert by["unattributed"]["model"] == "unknown" and by["unattributed"]["spend_usd"] == 3.0
    # invariant: per-provider spend sums to the window total
    assert round(sum(r["spend_usd"] for r in pb["rows"]), 2) == 10.0


# --- AC5-EDGE -----------------------------------------------------------------
def test_edge_zero_shipped_provider_null_cost_spend_shown():
    rows = [
        _row("glm", "glm-5", tr="NoProgress", cost=4.0),
        _row("glm", "glm-5", tr="Budget", cost=2.0),
        _row("claude", "opus", nid="x-1", cost=1.0),
    ]
    pb = build_provider_scoreboard(rows, GRAPH, since_days=28, now=NOW)
    glm = next(r for r in pb["rows"] if r["provider"] == "glm")
    assert glm["spend_usd"] == 6.0 and glm["shipped"] == 0
    assert glm["cost_per_shipped_usd"] is None
    assert round(sum(r["spend_usd"] for r in pb["rows"]), 2) == 7.0


def test_edge_junk_cost_and_iterations_never_crash():
    rows = [_row("claude", "opus", nid="x-1", cost="junk", iterations="junk")]
    pb = build_provider_scoreboard(rows, GRAPH, since_days=28, now=NOW)
    r = pb["rows"][0]
    assert r["spend_usd"] == 0.0 and r["median_iterations"] is None


def test_edge_unhashable_provider_model_nid_never_crash():
    # A row with list/dict values for the key fields folds into the fallback
    # buckets instead of raising TypeError (sigma silent-failure finding).
    rows = [
        {"type": "execution", "completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen",
         "cost_usd": 1.0, "provider_id": ["claude"], "model": {"id": "opus"}, "graph_node_id": ["x-1"]},
    ]
    pb = build_provider_scoreboard(rows, GRAPH, since_days=28, now=NOW)
    r = pb["rows"][0]
    assert (r["provider"], r["model"]) == ("unattributed", "unknown")
    assert r["shipped_linked"] == 0 and r["retry_rows"] == 0


def test_ui_populated_render_formats_bounce_and_blanks_repeated_provider(tmp_path, monkeypatch):
    ts = (datetime.now() - timedelta(days=1)).isoformat()
    graph = GRAPH + [{"id": "x-2", "caused_by": None}, {"id": "x-9", "caused_by": "x-1", "created_at": ts}]
    rows = [
        _row("claude", "opus", nid="x-1", cost=6.0, completed=ts),
        _row("claude", "haiku", nid="x-2", cost=2.0, completed=ts),
    ]
    (tmp_path / "graph.json").write_text(json.dumps({"entries": graph}))
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), ["--by-provider"])
    assert res.exit_code == 0, res.output
    assert "100% of 1" in res.output  # x-1 bounced (fix-node), denominator rides along
    assert "6.00" in res.output  # cost-per-shipped formatted
    # provider name printed once, blanked on its second model sub-row
    assert res.output.count("claude") == 1


# --- AC6-FR -------------------------------------------------------------------
def test_fr_corrupt_ledger_exit_1(tmp_path, monkeypatch):
    p = tmp_path / "ledger.json"
    p.write_text('{"entries": [ {corrupt')
    _wire(monkeypatch, tmp_path, p)
    res = runner.invoke(_app(), ["--by-provider"])
    assert res.exit_code == 1
    assert "ledger.json" in res.output and "byte" in res.output
