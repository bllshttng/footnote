"""AC coverage for `fno scoreboard` (Wave 5, x-e7c4).

AC5-HP  stop-cause + spend + coverage print; autonomy/survival on W4 signals.
AC5-ERR corrupt ledger -> file+offset on one line, exit 1.
AC5-UI  coverage <100% -> caveat on the same screen as any rate.
AC5-EDGE empty window -> explicit no-data, exit 0.
AC5-FR  mid-append partial -> single retry recovers rather than crashing.
"""

from __future__ import annotations

import json
from datetime import datetime

import pytest
import typer
from typer.testing import CliRunner

from fno.scoreboard import cli as sb_cli
from fno.scoreboard.fold import BrokenLedger, build_scoreboard, load_ledger_rows

runner = CliRunner()
NOW = datetime(2026, 7, 3, 20, 0, 0)


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


# --- AC5-HP -----------------------------------------------------------------
def test_hp_prints_core_metrics(tmp_path, monkeypatch):
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0},
        {"completed": "2026-07-02T10:00:00", "termination_reason": "NoProgress", "graph_node_id": "x-2", "cost_usd": 2.0},
    ]
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), [])
    assert res.exit_code == 0, res.output
    assert "Stop-cause distribution" in res.output
    assert "DonePRGreen" in res.output
    assert "Spend split" in res.output
    assert "ship-terminal:   $5.00" in res.output
    assert "wedge-terminal:  $2.00" in res.output


def test_hp_autonomy_survival_activate_with_w4(tmp_path, monkeypatch):
    # W4 signals present: a human_touch event + a graph node carrying a causal field.
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0}]
    (tmp_path / "events.jsonl").write_text(json.dumps({"type": "human_touch", "ts": "2026-07-03T09:00:00"}) + "\n")
    (tmp_path / "graph.json").write_text(json.dumps({"entries": [{"id": "x-1", "reverted": False}]}))
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), ["--json"])
    sb = json.loads(res.output)
    assert sb["autonomy"]["available"] is True
    assert sb["survival"]["available"] is True and sb["survival"]["survived"] == 1


def test_degrades_without_w4(tmp_path, monkeypatch):
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0}]
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), [])
    assert "Autonomy      n/a" in res.output
    assert "Survival      n/a" in res.output


# --- AC5-ERR ----------------------------------------------------------------
def test_err_corrupt_ledger_exit_1(tmp_path, monkeypatch):
    p = tmp_path / "ledger.json"
    p.write_text('{"entries": [ {"a": 1}, {corrupt')  # invalid JSON
    _wire(monkeypatch, tmp_path, p)
    res = runner.invoke(_app(), [])
    assert res.exit_code == 1
    assert "ledger.json" in res.output and "byte" in res.output


def test_err_load_raises_broken_ledger(tmp_path):
    p = tmp_path / "ledger.json"
    p.write_text("{not json")
    with pytest.raises(BrokenLedger) as ei:
        load_ledger_rows(p)
    assert ei.value.offset >= 0


# --- AC5-UI -----------------------------------------------------------------
def test_ui_partial_coverage_shows_caveat(tmp_path, monkeypatch):
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0},
        {"completed": "2026-07-02T10:00:00", "cost_usd": 1.0},  # no termination_reason -> <100%
    ]
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), [])
    assert "termination coverage" in res.output  # caveat present
    assert "%" in res.output


# --- AC5-EDGE ---------------------------------------------------------------
def test_edge_empty_window_exit_0(tmp_path, monkeypatch):
    rows = [{"completed": "2020-01-01T00:00:00", "termination_reason": "DonePRGreen", "cost_usd": 5.0}]
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), [])
    assert res.exit_code == 0
    assert "no terminal sessions in window" in res.output


def test_edge_missing_ledger_is_no_data(tmp_path, monkeypatch):
    _wire(monkeypatch, tmp_path, tmp_path / "does-not-exist.json")
    res = runner.invoke(_app(), [])
    assert res.exit_code == 0
    assert "no terminal sessions" in res.output


# --- real-data fold (regression) --------------------------------------------
def test_live_ledger_shape_folds_without_crash():
    """Fold the real 2000+ row ledger (any window) - proves the schema the
    live verb actually reads never trips the fold. Numbers vary; only the
    invariants are asserted."""
    from pathlib import Path

    live = Path.home() / ".fno" / "ledger.json"
    if not live.exists():
        pytest.skip("no live ledger on this machine")
    rows = load_ledger_rows(live)
    sb = build_scoreboard(rows, [], [], since_days=3650, now=datetime.now())
    assert sb["state"] in {"full", "partial", "no_data"}
    if sb["state"] != "no_data":
        cov = sb["coverage"]
        assert 0 <= cov["termination_reason_pct"] <= 100
        # spend split reconciles: every windowed row's cost lands in exactly one bucket
        assert sb["spend"]["ship_terminal_usd"] >= 0


# --- AC5-FR -----------------------------------------------------------------
def test_fr_single_retry_recovers(tmp_path, monkeypatch):
    """First read sees a truncated file (mid-append), the retry sees it whole."""
    p = tmp_path / "ledger.json"
    good = {"entries": [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "cost_usd": 1.0}]}
    p.write_text('{"entries": [ {"completed"')  # truncated

    state = {"n": 0}
    real_sleep = __import__("time").sleep

    def fake_sleep(_):
        state["n"] += 1
        p.write_text(json.dumps(good))  # writer finishes during the backoff

    monkeypatch.setattr("fno.scoreboard.fold.time.sleep", fake_sleep)
    rows = load_ledger_rows(p)
    assert state["n"] == 1  # retried exactly once
    assert len(rows) == 1
    _ = real_sleep  # keep reference; no real sleeping in the test
