"""Tests for the Codex/OpenCode generated-handle collision census."""

from __future__ import annotations

import json
import os
import sqlite3
from datetime import datetime, timezone
from pathlib import Path

import pytest

from fno.agents import handle_collisions


NOW = datetime(2026, 7, 30, 12, tzinfo=timezone.utc).timestamp()
RECENT = "2026-07-29T12:00:00Z"
OLD = "2026-07-01T12:00:00Z"


def _epoch_ms(timestamp: str) -> int:
    return int(datetime.fromisoformat(timestamp).timestamp() * 1000)


def _rollouts(root: Path, rows: list[tuple[str, str]]) -> None:
    root.mkdir(parents=True, exist_ok=True)
    for index, (session_id, timestamp) in enumerate(rows):
        payload = {
            "type": "session_meta",
            "payload": {"id": session_id, "timestamp": timestamp, "cwd": "/project"},
        }
        (root / f"rollout-{index}.jsonl").write_text(json.dumps(payload) + "\n")


def _database(path: Path, rows: list[tuple[str, object]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    con = sqlite3.connect(path)
    con.execute("CREATE TABLE session (id TEXT, time_created INTEGER)")
    con.executemany("INSERT INTO session VALUES (?, ?)", rows)
    con.commit()
    con.close()


def _census(tmp_path: Path, codex_rows, opencode_rows):
    codex = tmp_path / "codex"
    database = tmp_path / "opencode.db"
    _rollouts(codex, codex_rows)
    _database(database, [(sid, _epoch_ms(stamp)) for sid, stamp in opencode_rows])
    return handle_collisions.run_census(
        codex_sessions_dir=codex,
        opencode_db_path=database,
        opencode_storage_dir=tmp_path / "storage",
        window_days=7,
        now=NOW,
    )


def test_ac1_hp_measures_uuidv7_and_ses_prefix_clusters_without_duplicates(tmp_path):
    """AC1-HP: full/window metrics compare legacy last-eight with canonical first-eight."""
    codex_a = "019fb417-aaaa-7e73-b8b4-000000000001"
    opencode_a = "ses_abcd11111111"
    reports = _census(
        tmp_path,
        [
            (codex_a, RECENT),
            (codex_a, RECENT),
            ("019fb417-bbbb-7e73-b8b4-000000000002", RECENT),
            ("019fa000-cccc-7e73-b8b4-000000000003", OLD),
        ],
        [
            (opencode_a, RECENT),
            (opencode_a, RECENT),
            ("ses_abcd22222222", RECENT),
            ("ses_old_33333333", OLD),
        ],
    )

    for report in reports.values():
        assert report.full.session_count == 3
        assert report.full.legacy == handle_collisions.CollisionMetrics(0, 0, 1)
        assert report.full.canonical == handle_collisions.CollisionMetrics(1, 2, 2)
        assert report.window.session_count == 2
        assert report.window.legacy == handle_collisions.CollisionMetrics(0, 0, 1)
        assert report.window.canonical == handle_collisions.CollisionMetrics(1, 2, 2)


def test_ac1_edge_reads_legacy_opencode_created_time_when_database_is_absent(tmp_path):
    """AC1-EDGE: the supported legacy store fallback uses creation time."""
    codex = tmp_path / "codex"
    _rollouts(codex, [("019fb417-aaaa-7e73-b8b4-000000000001", RECENT)])
    storage = tmp_path / "opencode" / "storage"
    info = storage / "session" / "project" / "ses_abcd11111111.json"
    info.parent.mkdir(parents=True)
    info.write_text(
        json.dumps({"id": info.stem, "time": {"created": _epoch_ms(RECENT)}})
    )

    reports = handle_collisions.run_census(
        codex_sessions_dir=codex,
        opencode_db_path=tmp_path / "opencode" / "opencode.db",
        opencode_storage_dir=storage,
        window_days=7,
        now=NOW,
    )
    assert reports["opencode"].full.session_count == 1
    assert reports["opencode"].window.session_count == 1


def test_codex_census_refuses_partially_unreadable_tree(tmp_path):
    codex = tmp_path / "codex"
    _rollouts(
        codex / "visible",
        [("019fb417-aaaa-7e73-b8b4-000000000001", RECENT)],
    )
    blocked = codex / "blocked"
    _rollouts(
        blocked,
        [("019fb417-bbbb-7e73-b8b4-000000000002", RECENT)],
    )
    database = tmp_path / "opencode.db"
    _database(database, [("ses_one", _epoch_ms(RECENT))])

    blocked.chmod(0)
    try:
        if os.access(blocked, os.R_OK | os.X_OK):
            pytest.skip("test user can still enumerate mode-000 directories")
        with pytest.raises(handle_collisions.CorpusUnavailable, match="codex"):
            handle_collisions.run_census(
                codex_sessions_dir=codex,
                opencode_db_path=database,
                opencode_storage_dir=tmp_path / "storage",
                window_days=7,
                now=NOW,
            )
    finally:
        blocked.chmod(0o700)


def test_legacy_opencode_census_refuses_partially_unreadable_tree(tmp_path):
    codex = tmp_path / "codex"
    _rollouts(codex, [("019fb417-aaaa-7e73-b8b4-000000000001", RECENT)])
    storage = tmp_path / "opencode" / "storage"
    visible = storage / "session" / "visible"
    blocked = storage / "session" / "blocked"
    for directory, session_id in (
        (visible, "ses_abcd11111111"),
        (blocked, "ses_abcd22222222"),
    ):
        directory.mkdir(parents=True)
        (directory / f"{session_id}.json").write_text(
            json.dumps(
                {"id": session_id, "time": {"created": _epoch_ms(RECENT)}}
            )
        )

    blocked.chmod(0)
    try:
        if os.access(blocked, os.R_OK | os.X_OK):
            pytest.skip("test user can still enumerate mode-000 directories")
        with pytest.raises(handle_collisions.CorpusUnavailable, match="opencode"):
            handle_collisions.run_census(
                codex_sessions_dir=codex,
                opencode_db_path=tmp_path / "opencode" / "opencode.db",
                opencode_storage_dir=storage,
                window_days=7,
                now=NOW,
            )
    finally:
        blocked.chmod(0o700)


@pytest.mark.parametrize("kind", ["missing", "empty", "corrupt"])
def test_ac4_err_codex_unavailable_or_empty_is_not_reported_as_zero(tmp_path, kind):
    """AC4-ERR: missing, empty, and corrupt Codex corpora fail closed."""
    codex = tmp_path / "codex"
    if kind != "missing":
        codex.mkdir()
    if kind == "corrupt":
        (codex / "rollout-bad.jsonl").write_text("{broken")
    database = tmp_path / "opencode.db"
    _database(database, [("ses_one", _epoch_ms(RECENT))])

    with pytest.raises(handle_collisions.CorpusUnavailable, match="codex"):
        handle_collisions.run_census(
            codex_sessions_dir=codex,
            opencode_db_path=database,
            opencode_storage_dir=tmp_path / "storage",
            window_days=7,
            now=NOW,
        )


@pytest.mark.parametrize("kind", ["missing", "empty", "corrupt", "bad-record"])
def test_ac4_err_opencode_unavailable_or_empty_is_not_reported_as_zero(tmp_path, kind):
    """AC4-ERR: unavailable, empty, and malformed OpenCode data fails closed."""
    codex = tmp_path / "codex"
    _rollouts(codex, [("codex-one", RECENT)])
    database = tmp_path / "opencode" / "opencode.db"
    if kind == "empty":
        _database(database, [])
    elif kind == "corrupt":
        database.parent.mkdir(parents=True)
        database.write_text("not sqlite")
    elif kind == "bad-record":
        _database(database, [("ses_bad", "not-an-integer")])

    with pytest.raises(handle_collisions.CorpusUnavailable, match="opencode"):
        handle_collisions.run_census(
            codex_sessions_dir=codex,
            opencode_db_path=database,
            opencode_storage_dir=tmp_path / "storage",
            window_days=7,
            now=NOW,
        )


def test_ac1_hp_main_prints_every_metric_and_exits_zero_for_collision_free_tails(
    tmp_path, capsys
):
    """AC1-HP: the runnable command reports both rules and both scopes."""
    codex = tmp_path / "codex"
    database = tmp_path / "opencode.db"
    _rollouts(
        codex,
        [
            ("019fb417-aaaa-7e73-b8b4-000000000001", RECENT),
            ("019fa000-bbbb-7e73-b8b4-000000000001", RECENT),
        ],
    )
    _database(
        database,
        [("ses_abcd11111111", _epoch_ms(RECENT)), ("ses_bbcd11111111", _epoch_ms(RECENT))],
    )
    code = handle_collisions.main(
        ["--codex-sessions-dir", str(codex), "--opencode-db", str(database)],
        now=NOW,
    )

    output = capsys.readouterr().out
    assert code == 0
    assert output.count("\n") == 4
    assert "codex full sessions=2" in output
    assert "opencode window-7d sessions=2" in output
    for metric in (
        "legacy_colliding_handles=1",
        "legacy_affected_sessions=2",
        "legacy_worst_cluster=2",
        "canonical_colliding_handles=0",
        "canonical_affected_sessions=0",
        "canonical_worst_cluster=1",
    ):
        assert metric in output


def test_ac4_err_main_exits_nonzero_for_canonical_window_collision(tmp_path):
    """AC4-ERR: a canonical collision in either discovery window fails the check."""
    codex = tmp_path / "codex"
    database = tmp_path / "opencode.db"
    _rollouts(codex, [("codex-one", RECENT), ("codex-two", RECENT)])
    _database(database, [("ses_unique", _epoch_ms(RECENT))])
    code = handle_collisions.main(
        ["--codex-sessions-dir", str(codex), "--opencode-db", str(database)],
        now=NOW,
        canonical_fn=lambda _session_id: "collision",
    )
    assert code == 1


def test_ac4_err_main_emits_unavailable_not_zero_metrics(tmp_path, capsys):
    """AC4-ERR: unavailable evidence is named without fake zero rows."""
    code = handle_collisions.main(
        [
            "--codex-sessions-dir",
            str(tmp_path / "missing-codex"),
            "--opencode-db",
            str(tmp_path / "missing.db"),
            "--opencode-storage-dir",
            str(tmp_path / "missing-storage"),
        ],
        now=NOW,
    )
    captured = capsys.readouterr()
    assert code == 2
    assert "unavailable" in captured.err.lower()
    assert "sessions=0" not in captured.out
