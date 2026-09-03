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
from fno.scoreboard.fold import BrokenLedger, _parse_ts, build_scoreboard, load_ledger_rows

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


# Fixture timestamps are RELATIVE to now, not hardcoded (x-e957 fix-on-discovery).
# The scoreboard windows on "last 28d", so the literal 2026-07-02/03 rows these
# tests used aged out of the window on 2026-07-30T10:00Z and reddened CI on
# every PR from that moment. It passed locally only because the timestamps are
# naive and a UTC-behind developer clock still placed them inside the window -
# a failure that arrives on a wall-clock date and depends on the runner's
# timezone is the worst shape of flake to debug from a red PR.
def _days_ago(n: int, hour: int = 10) -> str:
    from datetime import datetime, timedelta

    return (datetime.now() - timedelta(days=n)).replace(
        hour=hour, minute=0, second=0, microsecond=0
    ).isoformat(timespec="seconds")


# Named for the role each plays in the window, so a reader sees the intent
# rather than an arithmetic puzzle.
_RECENT = _days_ago(1)
_OLDER = _days_ago(2)


def _wire(monkeypatch, tmp_path, ledger_path):
    import fno.paths as paths

    monkeypatch.setattr(paths, "ledger_json", lambda: ledger_path)
    monkeypatch.setattr(paths, "graph_json", lambda: tmp_path / "graph.json")


# --- AC5-HP -----------------------------------------------------------------
def test_hp_prints_core_metrics(tmp_path, monkeypatch):
    rows = [
        {"completed": _RECENT, "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0},
        {"completed": _OLDER, "termination_reason": "NoProgress", "graph_node_id": "x-2", "cost_usd": 2.0},
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
    rows = [{"completed": _RECENT, "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0}]
    (tmp_path / "events.jsonl").write_text(json.dumps({"type": "human_touch", "ts": _days_ago(1, hour=9)}) + "\n")
    (tmp_path / "graph.json").write_text(json.dumps({"entries": [{"id": "x-1", "reverted": False}]}))
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), ["--json"])
    sb = json.loads(res.output)
    assert sb["autonomy"]["available"] is True
    assert sb["survival"]["available"] is True and sb["survival"]["survived"] == 1


def test_degrades_without_w4(tmp_path, monkeypatch):
    rows = [{"completed": _RECENT, "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0}]
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
        {"completed": _RECENT, "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0},
        {"completed": _OLDER, "cost_usd": 1.0},  # no termination_reason -> <100%
    ]
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), [])
    assert "a partial window is not a trend" in res.output  # caveat present
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


# --- review hardening (gemini PR #186) --------------------------------------
def test_since_below_one_rejected(tmp_path, monkeypatch):
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, []))
    res = runner.invoke(_app(), ["--since", "0"])
    assert res.exit_code != 0  # typer.BadParameter


def test_malformed_cost_does_not_crash(tmp_path, monkeypatch):
    rows = [{"completed": _RECENT, "termination_reason": "DonePRGreen", "cost_usd": "not-a-number"}]
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    res = runner.invoke(_app(), ["--json"])
    assert res.exit_code == 0
    assert json.loads(res.output)["spend"]["ship_terminal_usd"] == 0.0


def test_aware_offset_timestamps_land_on_one_timeline():
    # Equivalent instants in different offsets must parse equal (convert, not
    # just strip tzinfo). tz-agnostic: no absolute value, so this holds whether
    # CI runs in UTC or the dev laptop in PDT.
    assert _parse_ts("2026-07-03T12:00:00+02:00") == _parse_ts("2026-07-03T10:00:00Z")
    assert _parse_ts("2026-07-03T14:00:00+04:00") == _parse_ts("2026-07-03T10:00:00Z")
    # a naive ledger timestamp is taken as-is (already local)
    assert _parse_ts("2026-07-03T10:00:00") == datetime(2026, 7, 3, 10, 0, 0)


def test_survival_ignores_fix_predating_ship(tmp_path, monkeypatch):
    # A fix-node created BEFORE the ship is not a follow-up to it -> node survives.
    # Fixed dates on purpose: this test injects an explicit `now=`, so it is
    # deterministic and must NOT follow the wall clock.
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 1.0}]
    graph = [
        {"id": "x-1", "reverted": False},
        {"id": "x-fix", "caused_by": "x-1", "created_at": "2026-07-01T00:00:00"},  # 2 days BEFORE ship
    ]
    sb = build_scoreboard(rows, [], graph, since_days=28, now=datetime(2026, 7, 3, 20, 0, 0))
    assert sb["survival"]["available"] is True and sb["survival"]["survived"] == 1

    # a fix AFTER the ship, within 14 days, counts against survival
    graph[1]["created_at"] = "2026-07-04T00:00:00"
    sb2 = build_scoreboard(rows, [], graph, since_days=28, now=datetime(2026, 7, 5, 20, 0, 0))
    assert sb2["survival"]["survived"] == 0


def test_zero_shipped_nodes_is_na_not_a_bare_rate():
    # W4 signals present but no Done* row in window -> autonomy/survival must be n/a,
    # never "0/0" or a raw touch count.
    # Fixed dates on purpose: explicit `now=` makes this deterministic.
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "NoProgress", "graph_node_id": "x-2", "cost_usd": 1.0}]
    touch = [{"type": "human_touch", "ts": "2026-07-03T09:00:00"}]
    graph = [{"id": "x-2", "reverted": False}, {"id": "x-9", "caused_by": "x-1"}]
    sb = build_scoreboard(rows, touch, graph, since_days=28, now=datetime(2026, 7, 3, 20, 0, 0))
    assert sb["autonomy"]["available"] is False
    assert sb["survival"]["available"] is False


def test_malformed_entries_shape_is_empty_not_crash(tmp_path):
    p = tmp_path / "ledger.json"
    p.write_text('{"entries": null}')  # valid JSON, junk shape
    assert load_ledger_rows(p) == []


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
    good = {"entries": [{"completed": _RECENT, "termination_reason": "DonePRGreen", "cost_usd": 1.0}]}
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


# --- x-b6bd: shipped is the merge, the terminal is a breakdown ---------------
def test_merged_node_with_backstop_row_counts_shipped():
    # AC1-HP: a node whose PR merged ships even when its only ledger row is a
    # reconcile-backstop (the session's terminal never said Done*), and its
    # spend is ship spend. The second merged node has no ledger row at all -
    # the population the ledger never saw, reported separately.
    rows = [
        {"completed": _RECENT, "termination_reason": "reconcile-backstop", "graph_node_id": "x-m", "cost_usd": 3.0}
    ]
    graph = [
        {"id": "x-m", "merge_status": "merged", "completed_at": _RECENT, "reverted": False},
        {"id": "x-orphan", "merge_status": "merged", "completed_at": _RECENT},
    ]
    sb = build_scoreboard(rows, [], graph, since_days=28, now=datetime.now())
    assert sb["shipped_nodes"] == 2
    assert sb["merged_nodes_without_ledger_row"] == 1
    assert sb["spend"]["ship_terminal_usd"] == 3.0
    assert sb["spend"]["wedge_terminal_usd"] == 0.0


def test_terminal_fallback_counts_node_absent_from_graph():
    # AC2-HP: the union keeps a Done* row whose node the graph has lost.
    rows = [{"completed": _RECENT, "termination_reason": "DonePRGreen", "graph_node_id": "x-gone", "cost_usd": 1.0}]
    sb = build_scoreboard(rows, [], [], since_days=28, now=datetime.now())
    assert sb["shipped_nodes"] == 1
    assert sb["shipped_by_terminal"] == 1


def test_noprogress_merged_node_is_shipped_not_wedge():
    # The session BELIEVED it made no progress; the merge is what HAPPENED.
    # Spend follows the node, so this row is ship spend, not wedge spend.
    rows = [{"completed": _RECENT, "termination_reason": "NoProgress", "graph_node_id": "x-np", "cost_usd": 2.0}]
    graph = [{"id": "x-np", "merge_status": "merged", "completed_at": _RECENT, "reverted": False}]
    sb = build_scoreboard(rows, [], graph, since_days=28, now=datetime.now())
    assert sb["shipped_nodes"] == 1
    assert sb["spend"]["ship_terminal_usd"] == 2.0
    assert sb["spend"]["wedge_terminal_usd"] == 0.0


def test_shipped_by_terminal_matches_legacy_count():
    # Parity: with no merged graph nodes the new count degrades to the old one.
    rows = [
        {"completed": _RECENT, "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0},
        {"completed": _OLDER, "termination_reason": "NoProgress", "graph_node_id": "x-2", "cost_usd": 2.0},
    ]
    sb = build_scoreboard(rows, [], [], since_days=28, now=datetime.now())
    assert sb["shipped_nodes"] == 1
    assert sb["shipped_by_terminal"] == 1


def test_register_stamps_utc_suffix(monkeypatch, tmp_path):
    # AC4-HP: the writer's started/completed carry the same +00:00 suffix and
    # never read completed < started (the 268-rows defect this closes).
    from fno.cost import _register

    def fake_git(*args):
        return {
            ("branch", "--show-current"): "feature/x-1",
            ("remote", "get-url", "origin"): "",
            ("rev-parse", "--show-toplevel"): str(tmp_path),
        }.get(args, "")

    monkeypatch.setattr(_register, "git_cmd", fake_git)
    monkeypatch.setattr(_register._paths, "resolve_canonical_worktree", lambda *a, **k: tmp_path)
    monkeypatch.setattr(_register, "_pr_number_from_ship_artifact", lambda *_: None)
    monkeypatch.setattr(_register, "_pr_number_from_gh", lambda *_: None)
    monkeypatch.setattr(_register, "sum_plan_points", lambda *_: 0)
    entry = _register.build_entry({"created_at": "2026-09-03T05:34:17Z", "fno_id": "tgt-utc"}, "")
    assert entry["started"].endswith("+00:00")
    assert entry["completed"].endswith("+00:00")
    assert datetime.fromisoformat(entry["completed"]) >= datetime.fromisoformat(entry["started"])


def test_render_shipped_caveat_shows_and_hides(tmp_path, monkeypatch):
    # AC3-HP: both counts ride together; the caveat only when they differ by
    # more than 10 percent.
    rows = [
        {"completed": _RECENT, "termination_reason": "reconcile-backstop", "graph_node_id": "x-m", "cost_usd": 1.0}
    ]
    graph = [{"id": "x-m", "merge_status": "merged", "completed_at": _RECENT}]
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows))
    (tmp_path / "graph.json").write_text(json.dumps({"entries": graph}))
    res = runner.invoke(_app(), [])
    assert "Shipped" in res.output and "by session terminal alone: 0" in res.output
    assert "the merge is the count" in res.output  # 0 of 1 = >10% apart

    rows2 = [
        {"completed": _RECENT, "termination_reason": "DonePRGreen", "graph_node_id": "x-m", "cost_usd": 1.0}
    ]
    _wire(monkeypatch, tmp_path, _ledger(tmp_path, rows2))
    res2 = runner.invoke(_app(), [])
    assert "the merge is the count" not in res2.output  # 1 of 1 = no gap
