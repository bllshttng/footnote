"""Unit tests for ``fno agents king checkin``.

The property under test: the printed numbers and the stored journal row come
from ONE dict, a failed reader never blanks a beat, and the diff reads the
previous canonical row through the history reader's own corpus.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest


def _fake_readers(*, overrides: dict | None = None):
    """Nine healthy readers with fixed numbers; `overrides` swaps by name."""
    base = {
        "user_notes": lambda scope: ("the operator asked for a verb\n", ""),
        "board": lambda scope: (
            {"open_prs": 5, "free_claim_no_driver": 2, "blocked": 1, "blocked_on": ["missing dependency"]},
            "",
        ),
        "blocked_child": lambda scope: (
            {"rows": [{"node": "x-eb79", "session": "cx-run-1", "age_minutes": 45, "reason": "worktree-init-blocked"}], "total": 1},
            "",
        ),
        "court": lambda scope: (
            {"active_nodes": 5, "total_nodes": 35, "rows": [{"id": "x-a1", "status": "in_progress", "worker": "w1", "pr_number": 7, "session": "s1"}]},
            "",
        ),
        "capacity": lambda scope: ({"footprint": "admit", "gate": "admit", "disagree": False, "unparsed_lines": 0}, ""),
        "workers": lambda scope: ({"live_workers": 12, "oldest_worker_seen": "476s t-x-0dc5"}, ""),
        "crown": lambda scope: ({"total": 5, "splits": 0, "disagreements": 0, "anomalies": []}, ""),
        "drain": lambda scope: (3, ""),
        "main_ci": lambda scope: ("green", ""),
    }
    merged = {**base, **(overrides or {})}
    return [(name, merged[name]) for name in
            ["user_notes", "board", "blocked_child", "court", "capacity",
             "workers", "crown", "drain", "main_ci"]]


def _read_journal(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


@pytest.fixture()
def checkin_env(monkeypatch, tmp_path):
    journal = tmp_path / "events.jsonl"
    monkeypatch.setattr("fno.paths.project_events_json", lambda: journal)
    monkeypatch.setattr("fno.paths.king_faqs_dir", lambda: tmp_path / "faqs")
    return journal


def test_row_numbers_equal_printed_numbers(checkin_env, capsys):
    from fno.king import checkin

    payload = checkin.run_checkin("x-37af", emit=True, readers=_fake_readers(), events_path=checkin_env)
    rows = [e for e in _read_journal(checkin_env) if e["type"] == "reign_checkin"]
    assert len(rows) == 1
    row_data = rows[0]["data"]

    assert payload["data"] == row_data
    assert row_data["scope"] == "x-37af"
    assert row_data["open_prs"] == 5
    assert row_data["blocked"] == 1
    assert row_data["undelivered"] == 3
    assert row_data["coverage"] == 9
    assert row_data["readers_failed"] == []

    out = capsys.readouterr().out
    assert "coverage: 9 of 9 readings ok" in out
    assert "main ci: green" in out
    assert "User notes:" in out
    assert "the operator asked for a verb" in out


def test_failed_reader_prints_names_and_refuses_no_change(checkin_env, capsys):
    from fno.king import checkin

    def boom(scope):
        raise checkin.ReaderError("graph unreadable")

    readers = _fake_readers(overrides={"drain": boom})
    payload = checkin.run_checkin("x-37af", emit=True, readers=readers, events_path=checkin_env)
    data = payload["data"]

    out = capsys.readouterr().out
    assert "READER FAILED drain: graph unreadable" in out
    assert "coverage: 8 of 9 readings ok" in out
    assert "drain" in out.split("coverage:")[0]

    assert data["coverage"] == 8
    assert data["readers_failed"] == ["drain"]
    assert "undelivered" not in data
    assert data["change"] != "no change"


def test_diff_against_previous_canonical_row(checkin_env, capsys):
    from fno.king import checkin

    previous = [{
        "ts": "2026-09-11T20:00:00Z",
        "type": "reign_checkin",
        "source": "loop",
        "data": {"scope": "x-37af", "change": "prior", "open_prs": 7, "blocked": 2, "undelivered": 3},
    }]
    monkey_target = "fno.king.checkin._history_rows"
    import fno.king.checkin as checkin_mod
    original = checkin_mod._history_rows
    checkin_mod._history_rows = lambda scope: previous  # monkeypatch without the fixture
    try:
        payload = checkin.run_checkin("x-37af", emit=False, readers=_fake_readers())
    finally:
        checkin_mod._history_rows = original

    out = capsys.readouterr().out
    assert "vs last beat (2026-09-11T20:00:00Z): open_prs 7 -> 5, blocked 2 -> 1" in out
    assert payload["data"]["change"] != "no change"
    assert "7 -> 5" in payload["data"]["change"]


def test_first_beat_names_no_previous(checkin_env, capsys):
    from fno.king import checkin

    checkin.run_checkin("x-37af", emit=False, readers=_fake_readers())
    out = capsys.readouterr().out
    assert "vs last beat: none, this is the first canonical beat for this scope" in out


def test_no_emit_appends_no_row(checkin_env, capsys):
    from fno.king import checkin

    payload = checkin.run_checkin("x-37af", emit=False, readers=_fake_readers())
    assert payload["emitted"] is False
    assert _read_journal(checkin_env) == []


def test_unmeasured_workers_never_read_as_zero(checkin_env, capsys):
    from fno.king import checkin

    def unreadable(scope):
        raise checkin.ReaderError("predicate absent from the top payload")

    readers = _fake_readers(overrides={"workers": unreadable})
    payload = checkin.run_checkin("x-37af", emit=True, readers=readers, events_path=checkin_env)
    out = capsys.readouterr().out

    assert "worker activity unmeasured: predicate absent from the top payload" in out
    assert "READER FAILED workers" in out
    assert "live_workers" not in payload["data"]
    assert payload["data"]["coverage"] == 8


def test_capacity_disagree_is_carried(checkin_env, capsys):
    from fno.king import checkin

    readers = _fake_readers(overrides={
        "capacity": lambda scope: ({"footprint": "admit", "gate": "refuse", "disagree": True, "unparsed_lines": 3}, ""),
    })
    payload = checkin.run_checkin("x-37af", emit=True, readers=readers, events_path=checkin_env)
    out = capsys.readouterr().out

    assert "DISAGREE" in out
    assert "unparsed_lines 3" in out
    assert payload["data"]["capacity_disagree"] is True
    assert payload["data"]["capacity_footprint"] == "admit"
    assert payload["data"]["capacity_gate"] == "refuse"


def test_no_change_requires_full_coverage_and_equal_numbers(checkin_env):
    import fno.king.checkin as checkin_mod
    from fno.king import checkin

    equal_previous = [{
        "ts": "2026-09-11T20:00:00Z",
        "type": "reign_checkin",
        "source": "loop",
        "data": {"scope": "x-37af", "change": "prior",
                 "open_prs": 5, "free_claim_no_driver": 2, "blocked": 1,
                 "active_nodes": 5, "live_workers": 12, "undelivered": 3},
    }]
    original = checkin_mod._history_rows
    checkin_mod._history_rows = lambda scope: equal_previous
    try:
        payload = checkin.run_checkin("x-37af", emit=False, readers=_fake_readers())
        assert payload["data"]["change"] == "no change"

        def boom(scope):
            raise checkin.ReaderError("down")

        payload = checkin.run_checkin("x-37af", emit=False, readers=_fake_readers(overrides={"drain": boom}))
        assert payload["data"]["change"] != "no change"
    finally:
        checkin_mod._history_rows = original


def test_checkin_is_registered_on_agents_king_app():
    from fno.king.cli import agents_king_app

    names = [cmd.name for cmd in agents_king_app.registered_commands]
    assert "checkin" in names


def test_r_court_rejects_unresolved_fold():
    from fno.king import checkin

    court = {"crowns": [{"scope": "x-s", "scope_nodes": {"status": "unresolved", "reason": "graph unreadable"}}]}
    with pytest.raises(checkin.ReaderError):
        checkin._r_court("x-s", lambda s: court)


def test_r_court_counts_active_rows_only():
    from fno.king import checkin

    court = {"crowns": [{"scope": "x-s", "scope_nodes": {"status": "ok", "total": 5, "nodes": [
        {"id": "a", "status": "in_progress"}, {"id": "b", "status": "blocked"},
    ]}}]}
    value, _ = checkin._r_court("x-s", lambda s: court)
    assert value["active_nodes"] == 2
    assert value["total_nodes"] == 5


def test_unexpected_reader_error_becomes_failed_reading(checkin_env):
    from fno.king import checkin

    def boom(scope):
        raise FileNotFoundError("gh")

    payload = checkin.run_checkin(
        "x-37af", emit=False,
        readers=_fake_readers(overrides={"main_ci": boom}),
        events_path=checkin_env,
    )
    assert payload["data"]["readers_failed"] == ["main_ci"]
    assert payload["data"]["coverage"] == 8
    assert "READER FAILED main_ci: FileNotFoundError: gh" in payload["lines"]
