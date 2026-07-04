"""AC coverage for `fno scoreboard --calibration` (W6 6.1, x-f063).

AC6-UI  >=10 verdicts -> confusion table; <10 -> "need more" line.
Denominator honesty: error / not_applicable / unattributed reported, never tabled.
"""

from __future__ import annotations

import json

import typer
from typer.testing import CliRunner

from fno.scoreboard import cli as sb_cli
from fno.scoreboard.fold import build_calibration

runner = CliRunner()


def _ev(nid, verdict):
    return {
        "ts": "2026-07-03T10:00:00Z",
        "type": "verifier_verdict",
        "source": "target",
        "data": {"graph_node_id": nid, "verdict": verdict, "source": "ship-gate"},
    }


def _ship_row(nid, completed="2026-07-01T10:00:00"):
    return {
        "completed": completed,
        "termination_reason": "DonePRGreen",
        "graph_node_id": nid,
        "cost_usd": 1.0,
    }


def _app():
    app = typer.Typer()
    app.command()(sb_cli.scoreboard_command)
    return app


def _wire(monkeypatch, tmp_path, ledger_path):
    import fno.paths as paths

    monkeypatch.setattr(paths, "ledger_json", lambda: ledger_path)
    monkeypatch.setattr(paths, "graph_json", lambda: tmp_path / "graph.json")


# --- fold: gating + honesty ---------------------------------------------------

def test_insufficient_below_ten():
    events = [_ev(f"x-{i}", "pass") for i in range(9)]
    cal = build_calibration(events, [], [])
    assert cal["state"] == "insufficient" and cal["n"] == 9 and cal["need"] == 10


def test_excluded_verdicts_do_not_count_toward_gate():
    # 9 countable + 3 excluded + 1 unattributed: still insufficient.
    events = [_ev(f"x-{i}", "pass") for i in range(9)]
    events += [_ev("x-e1", "error"), _ev("x-e2", "not_applicable"), _ev("x-e3", "error")]
    events += [_ev(None, "pass")]
    cal = build_calibration(events, [], [])
    assert cal["state"] == "insufficient" and cal["n"] == 9
    assert cal["excluded"] == {"error": 2, "not_applicable": 1}
    assert cal["unattributed"] == 1


def test_latest_verdict_per_node_wins():
    events = [_ev("x-1", "fail"), _ev("x-1", "pass")]
    events += [_ev(f"x-{i}", "concerns") for i in range(2, 11)]
    cal = build_calibration(events, [], [])
    assert cal["state"] == "ok" and cal["n"] == 10
    assert sum(cal["table"]["pass"].values()) == 1
    assert sum(cal["table"]["fail"].values()) == 0


def test_confusion_table_and_false_positive_rate():
    # 12 nodes: 8 pass (1 reverted, 1 bounced, 6 clean), 2 concerns, 2 fail.
    events = [_ev(f"x-p{i}", "pass") for i in range(8)]
    events += [_ev("x-c0", "concerns"), _ev("x-c1", "concerns")]
    events += [_ev("x-f0", "fail"), _ev("x-f1", "fail")]
    rows = [_ship_row(e["data"]["graph_node_id"]) for e in events]
    graph = [
        {"id": "x-p0", "reverted": True},
        # fix filed 2 days after ship -> bounced
        {"id": "x-fix", "caused_by": "x-p1", "created_at": "2026-07-03T10:00:00"},
        # fix predating the ship on a fail-verdict node -> NOT a bounce
        {"id": "x-old", "caused_by": "x-f0", "created_at": "2026-06-01T10:00:00"},
    ]
    cal = build_calibration(events, rows, graph)
    assert cal["state"] == "ok" and cal["n"] == 12
    assert cal["table"]["pass"] == {"merged_clean": 6, "bounced": 1, "reverted": 1}
    assert cal["table"]["concerns"] == {"merged_clean": 2, "bounced": 0, "reverted": 0}
    assert cal["table"]["fail"] == {"merged_clean": 2, "bounced": 0, "reverted": 0}
    assert cal["false_positive"] == {"count": 2, "of_pass": 8, "rate_pct": 25}


def test_untimeable_fix_counts_against_node():
    # No ship row for the node -> the caused_by fix can't be time-bounded ->
    # conservative bounced (mirrors _survival), and the untimed count is
    # surfaced so the conservative bias is visible (coverage honesty).
    events = [_ev(f"x-{i}", "pass") for i in range(10)]
    graph = [{"id": "x-fix", "caused_by": "x-0", "created_at": "2026-07-03T10:00:00"}]
    cal = build_calibration(events, [], graph)
    assert cal["table"]["pass"]["bounced"] == 1
    assert cal["untimed_outcomes"] == 10  # no ship rows at all in this fixture


def test_cli_untimed_caveat_rendered(tmp_path, monkeypatch):
    events = [_ev(f"x-{i}", "pass") for i in range(10)]
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": []}))  # no ship rows -> all untimed
    (tmp_path / "events.jsonl").write_text(
        "\n".join(json.dumps(e) for e in events) + "\n"
    )
    _wire(monkeypatch, tmp_path, ledger)
    res = runner.invoke(_app(), ["--calibration"])
    assert res.exit_code == 0, res.output
    assert "10 node(s) lack a timestamped ship row" in res.output


# --- CLI (AC6-UI) -------------------------------------------------------------

def test_cli_insufficient_prints_need_line(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": []}))
    (tmp_path / "events.jsonl").write_text(
        "\n".join(json.dumps(_ev(f"x-{i}", "pass")) for i in range(3)) + "\n"
    )
    _wire(monkeypatch, tmp_path, ledger)
    res = runner.invoke(_app(), ["--calibration"])
    assert res.exit_code == 0, res.output
    assert "3 verdicts so far, need >=10" in res.output


def test_cli_table_at_ten(tmp_path, monkeypatch):
    events = [_ev(f"x-{i}", "pass") for i in range(10)]
    rows = [_ship_row(f"x-{i}") for i in range(10)]
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": rows}))
    (tmp_path / "events.jsonl").write_text(
        "\n".join(json.dumps(e) for e in events) + "\n"
    )
    (tmp_path / "graph.json").write_text(
        json.dumps({"entries": [{"id": "x-0", "reverted": True}]})
    )
    _wire(monkeypatch, tmp_path, ledger)
    res = runner.invoke(_app(), ["--calibration"])
    assert res.exit_code == 0, res.output
    assert "N=10 verdicts" in res.output
    assert "merged_clean" in res.output and "reverted" in res.output
    assert "false-positive (pass -> bounced/reverted): 1/10 (10%)" in res.output


def test_cli_calibration_json(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": []}))
    _wire(monkeypatch, tmp_path, ledger)
    res = runner.invoke(_app(), ["--calibration", "--json"])
    assert res.exit_code == 0, res.output
    assert json.loads(res.output)["state"] == "insufficient"
