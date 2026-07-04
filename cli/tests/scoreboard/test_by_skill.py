"""AC coverage for `fno scoreboard --by-skill` (x-4829, loops roadmap W1).

AC-HP   28 days of real data prints rows for at least blueprint/do/review with
        non-zero runs and a coverage line.
AC-ERR  a session with no attributable skill records lands in an explicit
        "unattributed" row - never silently dropped.
AC-EDGE a skill file with no git history at the timestamp -> version "unknown",
        row still folds.
"""

from __future__ import annotations

import json

import typer
from typer.testing import CliRunner

from fno.scoreboard import cli as sb_cli
from fno.scoreboard.fold import build_skill_scoreboard

runner = CliRunner()


def _app():
    app = typer.Typer()
    app.command()(sb_cli.scoreboard_command)
    return app


def _wire(monkeypatch, tmp_path, ledger_path):
    import fno.paths as paths

    monkeypatch.setattr(paths, "ledger_json", lambda: ledger_path)
    monkeypatch.setattr(paths, "graph_json", lambda: tmp_path / "graph.json")


def _skill_line(skill: str) -> str:
    return json.dumps(
        {"message": {"content": [{"type": "tool_use", "name": "Skill", "input": {"skill": skill}}]}}
    )


UUID_A = "11111111-1111-1111-1111-111111111111"
UUID_B = "22222222-2222-2222-2222-222222222222"


# --- AC-HP -------------------------------------------------------------------
def test_hp_transcript_attribution_prints_runs_and_coverage():
    def read_transcript(sid):
        return {
            UUID_A: [_skill_line("fno:blueprint"), _skill_line("fno:do")],
            UUID_B: [_skill_line("fno:review")],
        }.get(sid)

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 6.0, "sessions": [UUID_A]},
        {"completed": "2026-07-02T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-2",
         "cost_usd": 2.0, "sessions": [UUID_B]},
    ]
    from datetime import datetime

    sb = build_skill_scoreboard(
        rows, [{"id": "x-1", "reverted": False}, {"id": "x-2", "reverted": False}], [],
        since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "v1",
    )
    assert sb["state"] == "ok"
    assert sb["coverage"] == {"rows": 2, "attributed_pct": 100}
    skills = {r["skill"] for r in sb["rows"]}
    assert {"fno:blueprint", "fno:do", "fno:review"} <= skills
    by_skill = {r["skill"]: r for r in sb["rows"]}
    assert by_skill["fno:review"]["runs"] == 1
    assert by_skill["fno:review"]["ship_rate_pct"] == 100
    assert by_skill["fno:review"]["method"] == "transcript"


def test_hp_cli_renders_coverage_and_table(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen",
         "graph_node_id": "x-1", "cost_usd": 3.0, "phases_completed": ["do", "review"]},
    ]}))
    _wire(monkeypatch, tmp_path, ledger)
    res = runner.invoke(_app(), ["--by-skill"])
    assert res.exit_code == 0, res.output
    assert "Coverage" in res.output and "attributed:" in res.output
    assert "fno:do" in res.output and "fno:review" in res.output
    assert "phase-proxy" in res.output


# --- AC-ERR ------------------------------------------------------------------
def test_err_unattributed_row_never_dropped():
    from datetime import datetime

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "NoProgress", "cost_usd": 1.0},
    ]
    sb = build_skill_scoreboard(
        rows, [], [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=lambda sid: None, resolve_skill_version=lambda s, ts: "v1",
    )
    assert sb["coverage"]["attributed_pct"] == 0
    assert len(sb["rows"]) == 1
    assert sb["rows"][0]["skill"] == "unattributed"
    assert sb["rows"][0]["runs"] == 1


def test_err_json_no_data_state(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": []}))
    _wire(monkeypatch, tmp_path, ledger)
    res = runner.invoke(_app(), ["--by-skill", "--json"])
    assert res.exit_code == 0, res.output
    assert json.loads(res.output)["state"] == "no_data"


# --- AC-EDGE -------------------------------------------------------------------
def test_edge_unresolvable_skill_version_is_unknown_and_folds():
    from datetime import datetime

    def read_transcript(sid):
        return [_skill_line("fno:some-deleted-skill")]

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 1.0, "sessions": [UUID_A]},
    ]
    sb = build_skill_scoreboard(
        rows, [{"id": "x-1", "reverted": False}], [],
        since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "unknown",
    )
    assert sb["rows"][0]["version"] == "unknown"
    assert sb["rows"][0]["runs"] == 1


def test_default_skill_version_missing_file_is_unknown(tmp_path, monkeypatch):
    from fno.scoreboard.fold import _default_skill_version

    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda: tmp_path)
    assert _default_skill_version("fno:no-such-skill", "2026-07-03T10:00:00") == "unknown"


# --- reverted attribution (mirrors calibration's outcome join) ---------------
def test_reverted_node_lowers_revert_rate_not_ship_rate():
    from datetime import datetime

    def read_transcript(sid):
        return [_skill_line("fno:do")]

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 1.0, "sessions": [UUID_A]},
        {"completed": "2026-07-02T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-2",
         "cost_usd": 1.0, "sessions": [UUID_A]},
    ]
    graph = [{"id": "x-1", "reverted": True}, {"id": "x-2", "reverted": False}]
    sb = build_skill_scoreboard(
        rows, graph, [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "v1",
    )
    row = sb["rows"][0]
    assert row["ship_rate_pct"] == 100  # both rows shipped
    assert row["revert_rate_pct"] == 50  # 1 of 2 shipped rows reverted
