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
    # no fractional split: a row that named 2 skills counts its full cost
    # toward EACH of them (documented v1 approximation).
    assert by_skill["fno:blueprint"]["cost_per_run"] == 6.0
    assert by_skill["fno:do"]["cost_per_run"] == 6.0


def test_hp_touch_events_join_touches_per_run():
    from datetime import datetime

    def read_transcript(sid):
        return [_skill_line("fno:do")]

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 1.0, "sessions": [UUID_A]},
    ]
    touches = [
        {"type": "human_touch", "graph_node_id": "x-1", "ts": "2026-07-03T09:00:00"},
        {"type": "human_touch", "data": {"graph_node_id": "x-1"}, "ts": "2026-07-03T09:30:00"},
        {"type": "human_touch", "graph_node_id": "x-other", "ts": "2026-07-03T09:00:00"},
        # codex peer review finding: a touch outside --since must not count,
        # even though it's for the right node.
        {"type": "human_touch", "graph_node_id": "x-1", "ts": "2020-01-01T00:00:00"},
    ]
    sb = build_skill_scoreboard(
        rows, [{"id": "x-1", "reverted": False}], touches,
        since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "v1",
    )
    assert sb["rows"][0]["touches_per_run"] == 2.0  # only x-1's 2 in-window touches count


def test_hp_since_days_window_excludes_old_rows():
    from datetime import datetime

    def read_transcript(sid):
        return [_skill_line("fno:do")]

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 1.0, "sessions": [UUID_A]},
        {"completed": "2020-01-01T00:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-old",
         "cost_usd": 99.0, "sessions": [UUID_A]},
    ]
    sb = build_skill_scoreboard(
        rows, [], [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "v1",
    )
    assert sb["coverage"]["rows"] == 1  # the 2020 row is outside the 28-day window
    assert sb["rows"][0]["runs"] == 1


def test_default_hooks_real_path_never_crashes(tmp_path, monkeypatch):
    # The default (non-injected) read_transcript / resolve_skill_version hooks
    # are what the CLI actually uses; exercise them for real instead of 100%
    # mocking them out. A session id with no matching transcript file, and a
    # skill with no matching SKILL.md, must both degrade cleanly.
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda: tmp_path)
    from datetime import datetime

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 1.0, "sessions": [UUID_A], "phases_completed": ["do"]},
    ]
    sb = build_skill_scoreboard(rows, [], [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0))
    assert sb["state"] == "ok"
    assert sb["rows"][0]["skill"] == "fno:do"  # no transcript found -> phase-proxy fallback
    assert sb["rows"][0]["version"] == "unknown"  # no skills/do/SKILL.md under the fake repo root


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


def test_err_unattributed_row_keeps_its_own_cost_not_zero():
    # silent-failure-hunter finding: the unattributed bucket must accumulate the
    # row's own cost/touches, not silently render $0.00 as if that were measured.
    from datetime import datetime

    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "NoProgress", "cost_usd": 7.0}]
    sb = build_skill_scoreboard(
        rows, [], [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=lambda sid: None, resolve_skill_version=lambda s, ts: "v1",
    )
    assert sb["rows"][0]["cost_per_run"] == 7.0


def test_err_malformed_transcript_line_never_crashes():
    # A valid-JSON, junk-shape line (bare scalar) or a Skill block with a
    # non-dict `input` must degrade like every other line, never crash the fold.
    from datetime import datetime

    def read_transcript(sid):
        return [
            "42",  # valid JSON, not a dict
            json.dumps({"message": "not-a-dict"}),  # gemini finding: message must not be trusted as a dict
            _skill_line("fno:do"),
            json.dumps({"message": {"content": [{"type": "tool_use", "name": "Skill", "input": "not-a-dict"}]}}),
        ]

    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "sessions": [UUID_A]}]
    sb = build_skill_scoreboard(
        rows, [{"id": "x-1", "reverted": False}], [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "v1",
    )
    assert sb["rows"][0]["skill"] == "fno:do"  # the one real Skill block still folds


def test_err_non_dict_touch_event_data_never_crashes():
    # gemini finding: e.get("data") may be a non-dict; must not raise on .get.
    from datetime import datetime

    def read_transcript(sid):
        return [_skill_line("fno:do")]

    rows = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
             "cost_usd": 1.0, "sessions": [UUID_A]}]
    touches = ["not-a-dict", {"type": "human_touch", "data": "also-not-a-dict"}]
    sb = build_skill_scoreboard(
        rows, [{"id": "x-1", "reverted": False}], touches, since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "v1",
    )
    assert sb["rows"][0]["touches_per_run"] == 0.0


def test_err_non_list_sessions_and_phases_never_crashes():
    # gemini finding: `sessions`/`phases_completed` may not be lists at all.
    from datetime import datetime

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "NoProgress", "cost_usd": 1.0,
         "sessions": "not-a-list", "phases_completed": "also-not-a-list"},
    ]
    sb = build_skill_scoreboard(
        rows, [], [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=lambda sid: None, resolve_skill_version=lambda s, ts: "v1",
    )
    assert sb["rows"][0]["skill"] == "unattributed"


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


def test_shipped_row_without_node_id_excluded_from_revert_denominator():
    # silent-failure-hunter finding: a shipped row with no graph_node_id can
    # never resolve an outcome, so it must not silently pad the "not reverted"
    # side of revert_rate_pct (that would make an unlinked skill look safer
    # than it's actually known to be).
    from datetime import datetime

    def read_transcript(sid):
        return [_skill_line("fno:do")]

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 1.0, "sessions": [UUID_A]},
        {"completed": "2026-07-02T10:00:00", "termination_reason": "DonePRGreen", "cost_usd": 1.0,  # no graph_node_id
         "sessions": [UUID_A]},
    ]
    graph = [{"id": "x-1", "reverted": True}]
    sb = build_skill_scoreboard(
        rows, graph, [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "v1",
    )
    row = sb["rows"][0]
    assert row["ship_rate_pct"] == 100  # both rows shipped
    # denominator is shipped_linked (1), not shipped (2): the unlinked row
    # must not dilute the rate toward "safe".
    assert row["revert_rate_pct"] == 100


def test_revert_rate_is_na_not_zero_without_causal_telemetry():
    # codex peer review finding: a shipped+linked row whose graph carries NO
    # causal telemetry at all (no node anywhere has `reverted`/`caused_by`)
    # must render "n/a", not a bare 0% that looks identical to a real clean
    # record (mirrors _survival's own w4 gate in build_scoreboard).
    from datetime import datetime

    def read_transcript(sid):
        return [_skill_line("fno:do")]

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 1.0, "sessions": [UUID_A]},
    ]
    graph = [{"id": "x-1"}]  # exists, but carries no causal telemetry anywhere
    sb = build_skill_scoreboard(
        rows, graph, [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "v1",
    )
    assert sb["rows"][0]["revert_rate_pct"] is None


def test_revert_rate_is_na_when_node_missing_from_graph():
    # A graph_node_id present on the row but absent from graph_nodes entirely
    # (deleted node, stale data) must not resolve to a false "merged_clean".
    from datetime import datetime

    def read_transcript(sid):
        return [_skill_line("fno:do")]

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-missing",
         "cost_usd": 1.0, "sessions": [UUID_A]},
    ]
    graph = [{"id": "x-other", "reverted": True}]  # w4 telemetry exists, but not for THIS node
    sb = build_skill_scoreboard(
        rows, graph, [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "v1",
    )
    assert sb["rows"][0]["revert_rate_pct"] is None


def test_cli_renders_na_for_unjudgeable_revert_rate(tmp_path, monkeypatch):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen",
         "graph_node_id": "x-1", "cost_usd": 3.0, "phases_completed": ["do"]},
    ]}))
    _wire(monkeypatch, tmp_path, ledger)  # no graph.json -> no causal telemetry
    res = runner.invoke(_app(), ["--by-skill"])
    assert res.exit_code == 0, res.output
    assert "n/a" in res.output


def test_mixed_attribution_methods_labeled_not_last_write_wins():
    from datetime import datetime

    def read_transcript(sid):
        return {UUID_A: [_skill_line("fno:do")]}.get(sid)

    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 1.0, "sessions": [UUID_A]},
        {"completed": "2026-07-02T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-2",
         "cost_usd": 1.0, "phases_completed": ["do"]},  # no transcript session -> phase-proxy for the same skill
    ]
    graph = [{"id": "x-1", "reverted": False}, {"id": "x-2", "reverted": False}]
    sb = build_skill_scoreboard(
        rows, graph, [], since_days=28, now=datetime(2026, 7, 3, 20, 0, 0),
        read_transcript=read_transcript, resolve_skill_version=lambda s, ts: "v1",
    )
    row = sb["rows"][0]
    assert row["skill"] == "fno:do" and row["runs"] == 2
    assert row["method"] == "phase-proxy+transcript"


def test_skill_commit_history_shells_out_once_then_caches(monkeypatch, tmp_path):
    # gemini + code-reviewer finding: resolving a skill's version must not
    # shell out to git once per row. _skill_commit_history now fetches the
    # full log once per (root, path) and every later call for the same pair
    # is served from the in-memory cache.
    import subprocess as real_subprocess

    from fno.scoreboard.fold import _SKILL_COMMIT_HISTORY_CACHE, _skill_commit_history

    _SKILL_COMMIT_HISTORY_CACHE.clear()
    calls = {"n": 0}
    real_run = real_subprocess.run

    def counting_run(*args, **kwargs):
        calls["n"] += 1
        return real_run(*args, **kwargs)

    monkeypatch.setattr(real_subprocess, "run", counting_run)

    _skill_commit_history(tmp_path, "no/such/file.md")
    _skill_commit_history(tmp_path, "no/such/file.md")
    _skill_commit_history(tmp_path, "no/such/file.md")
    assert calls["n"] == 1  # 2nd and 3rd calls hit the cache, no new subprocess
