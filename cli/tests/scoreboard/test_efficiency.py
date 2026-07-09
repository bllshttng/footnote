"""AC coverage for `fno scoreboard --efficiency` (x-c284).

AC1-HP   efficiency view renders per-outcome-class buckets + distribution lines
         + a coverage dict from recorded telemetry.
AC2-HP   ci_reds counts red EPISODES (transitions into FAILURE), not red polls.
AC3-ERR  a corrupt events line never crashes the fold.
AC4-UI   no-data prints a state, and --efficiency pairs exclusively.
AC5-EDGE a session with no joined events -> None (not 0), excluded from dists.
AC6-FR   an unrecognized ci value surfaces as ci_unparsed, ci_reds None.
"""

from __future__ import annotations

import json
from datetime import datetime

import typer
from typer.testing import CliRunner

from fno.scoreboard import cli as sb_cli
from fno.scoreboard.fold import build_efficiency

runner = CliRunner()
NOW = datetime(2026, 7, 3, 20, 0, 0)


def _app():
    app = typer.Typer()
    app.command()(sb_cli.scoreboard_command)
    return app


def _wire(monkeypatch, tmp_path, ledger_path, events=None):
    import fno.paths as paths

    monkeypatch.setattr(paths, "ledger_json", lambda: ledger_path)
    monkeypatch.setattr(paths, "graph_json", lambda: tmp_path / "graph.json")
    (ledger_path.parent / "events.jsonl").write_text("\n".join(events or []) + ("\n" if events else ""))


def _loop(sid: str, ci: str, ts: str) -> str:
    return json.dumps({"ts": ts, "type": "loop_check", "data": {"session_id": sid, "ci": ci}})


# --- AC1-HP ------------------------------------------------------------------
def test_hp_efficiency_view_shape():
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 5.0, "tokens_total": 1000, "duration_minutes": 30, "sessions": ["s-a"]},
        {"completed": "2026-07-02T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-2",
         "cost_usd": 8.0, "tokens_total": 4000, "duration_minutes": 90, "sessions": ["s-b"]},
    ]
    events = [
        {"ts": "2026-07-03T09:00:00Z", "type": "loop_check", "data": {"session_id": "s-a", "ci": "SUCCESS"}},
        {"ts": "2026-07-02T09:00:00Z", "type": "loop_check", "data": {"session_id": "s-b", "ci": "FAILURE:smoke"}},
        {"ts": "2026-07-02T09:05:00Z", "type": "loop_check", "data": {"session_id": "s-b", "ci": "SUCCESS"}},
    ]
    graph = [{"id": "x-1", "reverted": False}, {"id": "x-2", "reverted": False}]
    eff = build_efficiency(rows, events, graph, since_days=28, now=NOW, read_transcript=lambda sid: None)

    assert eff["state"] == "ok"
    cov = eff["coverage"]
    assert set(cov) >= {"rows", "loop_join_pct", "transcript_pct", "node_linkage_pct", "ci_unparsed"}
    assert cov["rows"] == 2 and cov["loop_join_pct"] == 100 and cov["node_linkage_pct"] == 100
    mc = eff["per_outcome_class"]["merged_clean"]
    assert mc["n"] == 2 and mc["spend_usd"] == 13.0 and mc["median_fires"] is not None
    dist = eff["distribution"]
    assert set(dist) == {"loop_fires", "ci_reds", "tokens_total", "duration_minutes"}
    for m in dist.values():
        assert set(m) == {"median", "p90", "n"}
    assert dist["ci_reds"]["median"] in (0, 1)  # s-a 0 reds, s-b 1 red


# --- AC2-HP ------------------------------------------------------------------
def test_hp_ci_reds_counts_episodes_not_polls():
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "tokens_total": 1, "duration_minutes": 1, "sessions": ["s-a"]}]
    # SUCCESS, FAILURE:smoke, FAILURE:smoke, SUCCESS -> one red episode.
    events = [
        _loop("s-a", "SUCCESS", "2026-07-03T09:00:00Z"),
        _loop("s-a", "FAILURE:smoke", "2026-07-03T09:01:00Z"),
        _loop("s-a", "FAILURE:smoke", "2026-07-03T09:02:00Z"),
        _loop("s-a", "SUCCESS", "2026-07-03T09:03:00Z"),
    ]
    events = [json.loads(e) for e in events]
    eff = build_efficiency(rows, events, [{"id": "x-1", "reverted": False}],
                           since_days=28, now=NOW, read_transcript=lambda sid: None)
    assert eff["distribution"]["ci_reds"]["median"] == 1


def test_hp_two_distinct_red_episodes_count_twice():
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "sessions": ["s-a"]}]
    events = [json.loads(_loop("s-a", ci, f"2026-07-03T09:0{i}:00Z"))
              for i, ci in enumerate(["FAILURE:a", "SUCCESS", "FAILURE:b", "SUCCESS"])]
    eff = build_efficiency(rows, events, [{"id": "x-1", "reverted": False}],
                           since_days=28, now=NOW, read_transcript=lambda sid: None)
    assert eff["distribution"]["ci_reds"]["median"] == 2


# --- AC3-ERR -----------------------------------------------------------------
def test_err_corrupt_events_line_never_crashes(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen",
         "graph_node_id": "x-1", "cost_usd": 1.0, "sessions": ["s-a"]},
    ]}))
    events = [
        _loop("s-a", "FAILURE:smoke", "2026-07-03T09:00:00Z"),
        "{ this is not valid json",  # corrupt line between valid ones
        _loop("s-a", "SUCCESS", "2026-07-03T09:01:00Z"),
    ]
    _wire(monkeypatch, tmp_path, ledger, events)
    res = runner.invoke(_app(), ["--efficiency", "--json"])
    assert res.exit_code == 0, res.output
    d = json.loads(res.output)
    assert d["state"] == "ok"
    assert d["coverage"]["loop_join_pct"] == 100  # both valid events counted


# --- AC4-UI ------------------------------------------------------------------
def test_ui_no_data_state(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": []}))
    _wire(monkeypatch, tmp_path, ledger)
    res = runner.invoke(_app(), ["--efficiency"])
    assert res.exit_code == 0, res.output
    assert "no terminal sessions" in res.output
    res_json = runner.invoke(_app(), ["--efficiency", "--json"])
    assert json.loads(res_json.output)["state"] == "no_data"


def test_ui_efficiency_conflicts_with_calibration(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": []}))
    _wire(monkeypatch, tmp_path, ledger)
    res = runner.invoke(_app(), ["--efficiency", "--calibration"])
    assert res.exit_code != 0
    assert "mutually exclusive" in res.output


def test_ui_efficiency_conflicts_with_by_skill(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": []}))
    _wire(monkeypatch, tmp_path, ledger)
    res = runner.invoke(_app(), ["--efficiency", "--by-skill"])
    assert res.exit_code != 0
    assert "mutually exclusive" in res.output


def test_ui_cli_renders_coverage_first(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen",
         "graph_node_id": "x-1", "cost_usd": 3.0, "tokens_total": 10, "duration_minutes": 5, "sessions": ["s-a"]},
    ]}))
    _wire(monkeypatch, tmp_path, ledger, [_loop("s-a", "SUCCESS", "2026-07-03T09:00:00Z")])
    res = runner.invoke(_app(), ["--efficiency"])
    assert res.exit_code == 0, res.output
    body = res.output
    assert body.index("Coverage") < body.index("Per-outcome-class") < body.index("Distribution")


# --- AC5-EDGE ----------------------------------------------------------------
def test_edge_no_events_is_none_not_zero():
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "tokens_total": 5, "duration_minutes": 3, "sessions": ["s-none"]}]
    eff = build_efficiency(rows, [], [{"id": "x-1", "reverted": False}],
                           since_days=28, now=NOW, read_transcript=lambda sid: None)
    # the row joined no loop_check event: excluded from every distribution,
    # coverage reflects the miss.
    assert eff["coverage"]["loop_join_pct"] == 0
    assert eff["distribution"]["loop_fires"] == {"median": None, "p90": None, "n": 0}
    assert eff["distribution"]["ci_reds"]["n"] == 0
    # but the row still lands in its outcome bucket (spend reconciles).
    assert eff["per_outcome_class"]["merged_clean"]["n"] == 1
    assert eff["per_outcome_class"]["merged_clean"]["median_fires"] is None


def test_edge_single_row_median_equals_p90():
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "tokens_total": 42, "duration_minutes": 7, "sessions": ["s-a"]}]
    events = [json.loads(_loop("s-a", "SUCCESS", "2026-07-03T09:00:00Z"))]
    eff = build_efficiency(rows, events, [{"id": "x-1", "reverted": False}],
                           since_days=28, now=NOW, read_transcript=lambda sid: None)
    d = eff["distribution"]["tokens_total"]
    assert d["median"] == d["p90"] == 42  # no divide-by-zero, both = the value


# --- AC6-FR ------------------------------------------------------------------
def test_fr_unrecognized_ci_surfaces_as_unparsed_not_silence():
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "sessions": ["s-a"]}]
    events = [
        json.loads(_loop("s-a", "SUCCESS", "2026-07-03T09:00:00Z")),
        json.loads(_loop("s-a", "WEIRD_NEW_SHAPE", "2026-07-03T09:01:00Z")),
    ]
    eff = build_efficiency(rows, events, [{"id": "x-1", "reverted": False}],
                           since_days=28, now=NOW, read_transcript=lambda sid: None)
    assert eff["coverage"]["ci_unparsed"] == 1
    # the affected session's ci_reds is None (excluded from the distribution),
    # never a fabricated count.
    assert eff["distribution"]["ci_reds"]["n"] == 0


def test_fr_known_unknown_and_skipped_are_not_drift():
    # plan deviation: unknown/skipped are real emitter values, recognized-benign,
    # NOT counted as ci_unparsed (else most real sessions would flag drift).
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "sessions": ["s-a"]}]
    events = [
        json.loads(_loop("s-a", "unknown", "2026-07-03T09:00:00Z")),
        json.loads(_loop("s-a", "skipped", "2026-07-03T09:01:00Z")),
        json.loads(_loop("s-a", "SUCCESS", "2026-07-03T09:02:00Z")),
    ]
    eff = build_efficiency(rows, events, [{"id": "x-1", "reverted": False}],
                           since_days=28, now=NOW, read_transcript=lambda sid: None)
    assert eff["coverage"]["ci_unparsed"] == 0
    assert eff["distribution"]["ci_reds"]["median"] == 0  # no red episodes


# --- join semantics ----------------------------------------------------------
def test_join_falls_back_to_scalar_session_id():
    # a row predating the `sessions` list carries only the scalar session_id.
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "session_id": "legacy-sid"}]
    events = [json.loads(_loop("legacy-sid", "FAILURE:smoke", "2026-07-03T09:00:00Z"))]
    eff = build_efficiency(rows, events, [{"id": "x-1", "reverted": False}],
                           since_days=28, now=NOW, read_transcript=lambda sid: None)
    assert eff["coverage"]["loop_join_pct"] == 100
    assert eff["distribution"]["loop_fires"]["median"] == 1


def test_shipped_without_causal_telemetry_is_untracked_not_clean():
    # no node carries reverted/caused_by anywhere -> a shipped row must not
    # silently claim merged_clean (mirrors _survival's w4 gate).
    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "sessions": ["s-a"]}]
    events = [json.loads(_loop("s-a", "SUCCESS", "2026-07-03T09:00:00Z"))]
    eff = build_efficiency(rows, events, [{"id": "x-1"}],  # exists, no causal fields
                           since_days=28, now=NOW, read_transcript=lambda sid: None)
    assert "merged_clean" not in eff["per_outcome_class"]
    assert eff["per_outcome_class"]["shipped_untracked"]["n"] == 1
    assert eff["coverage"]["outcome_tracked_pct"] == 0


def test_missing_tokens_duration_are_none_not_zero():
    # codex P2: a row missing tokens_total/duration_minutes (finalize records
    # None when transcript cost extraction is unavailable) must NOT be coerced to
    # 0 and pollute the median/p90 populations with fake zero-token sessions.
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 1.0, "tokens_total": 1000, "duration_minutes": 30, "sessions": ["s-a"]},
        {"completed": "2026-07-02T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-2",
         "cost_usd": 2.0, "sessions": ["s-b"]},  # no tokens_total / duration_minutes
    ]
    events = [
        json.loads(_loop("s-a", "SUCCESS", "2026-07-03T09:00:00Z")),
        json.loads(_loop("s-b", "SUCCESS", "2026-07-02T09:00:00Z")),
    ]
    eff = build_efficiency(rows, events, [{"id": "x-1", "reverted": False}, {"id": "x-2", "reverted": False}],
                           since_days=28, now=NOW, read_transcript=lambda sid: None)
    # both rows fired, so both are in the distribution population - but only the
    # measured row contributes to token/duration medians (n=1), never a fake 0.
    assert eff["distribution"]["tokens_total"] == {"median": 1000, "p90": 1000, "n": 1}
    assert eff["distribution"]["duration_minutes"] == {"median": 30, "p90": 30, "n": 1}
    assert eff["distribution"]["loop_fires"]["n"] == 2  # loop_fires still measured for both
    # per-outcome-class median tokens is the measured value, not a 0-diluted one.
    assert eff["per_outcome_class"]["merged_clean"]["median_tokens"] == 1000


def test_transcript_counts_feed_coverage():
    def read_transcript(sid):
        return [
            json.dumps({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "Bash"}, {"type": "text", "text": "hi"}]}}),
            json.dumps({"type": "user", "message": {"content": [{"type": "tool_result"}]}}),
        ]

    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "sessions": ["s-a"]}]
    eff = build_efficiency(rows, [], [{"id": "x-1", "reverted": False}],
                           since_days=28, now=NOW, read_transcript=read_transcript)
    assert eff["coverage"]["transcript_pct"] == 100
