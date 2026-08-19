"""`fno backlog board`: three sections, zero GitHub reads.

The guarantee under test is "zero API calls", proved with a positive-failure
instrument rather than an absence: subprocess.run is monkeypatched to RAISE
on any invocation, so a network read becomes a loud failure with a traceback
naming the caller. A test that merely counts calls and asserts zero cannot
tell "nothing ran" from "the instrument was never wired" (AGENTS.md pitfalls
corpus, "assert a positive marker, never an absence").

Filter: `uv run --project cli pytest cli/tests/graph/test_board.py -q`
"""
from __future__ import annotations

import ast
import inspect
import json
import subprocess
from datetime import datetime, timedelta, timezone

import pytest
from typer.testing import CliRunner

from fno.graph import board as board_module
from fno.graph.board import compute_board
from fno.graph.cli import cli

runner = CliRunner()

NOW = datetime(2026, 8, 18, 12, 0, 0, tzinfo=timezone.utc)


def _iso(dt: datetime) -> str:
    return dt.isoformat()


def _node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "title": f"node {node_id}",
        "project": "fno",
        "type": "feature",
        "parent": None,
        "priority": "p2",
        "status": "ready",
        "blocked_by": [],
        "completed_at": None,
        "pr_number": None,
        "pr_url": None,
        "created_at": _iso(NOW - timedelta(days=1)),
    }
    base.update(overrides)
    return base


@pytest.fixture(autouse=True)
def _no_live_claims(monkeypatch):
    """Never touch the real global claims root from a unit test."""
    monkeypatch.setattr(board_module, "live_claimed_node_ids", lambda **_kw: set())


@pytest.fixture
def raising_subprocess(monkeypatch):
    """The positive-failure instrument: any subprocess.run call fails loudly."""

    def _raise(*args, **kwargs):
        raise AssertionError(f"board must never shell out: subprocess.run{args!r}")

    monkeypatch.setattr(subprocess, "run", _raise)
    return _raise


@pytest.fixture
def cache_dir(tmp_path, monkeypatch):
    d = tmp_path / "pr-status-cache"
    d.mkdir()
    monkeypatch.setenv("FNO_PR_STATUS_CACHE_DIR", str(d))
    return d


def _write_cache_row(cache_dir, slug_key: str, pr: str, sha: str, output: dict, ts: float) -> None:
    (cache_dir / f"{slug_key}-{pr}-{sha}.json").write_text(
        json.dumps({"ts": ts, "exit": 0, "output": output, "fail_count": 0, "backoff_until": 0}),
        encoding="utf-8",
    )


# ---------------------------------------------------------------------------
# The quiet-source proof
# ---------------------------------------------------------------------------


def test_zero_subprocess_calls_over_a_full_fixture(raising_subprocess, cache_dir):
    """Given a full fixture (all three sections populated, cached PR rows
    present), when compute_board runs with subprocess.run wired to raise,
    then it completes and prints all three sections without ever invoking it."""
    _write_cache_row(
        cache_dir, "acme--widgets", "951", "deadbeef0000",
        {"verdict": "red", "green": False}, ts=NOW.timestamp() - 3600,
    )
    entries = [
        _node(
            "x-done1", title="just shipped it", completed_at=_iso(NOW - timedelta(hours=2)),
            pr_number=937,
        ),
        _node(
            "x-prog1", title="provider outage failover", status="in_progress",
            pr_number=951, pr_url="https://github.com/acme/widgets/pull/951",
        ),
        _node("x-deck1", title="on deck node"),
    ]

    board = compute_board(entries, project="fno", now=NOW)

    assert board["just_finished"]["rows"][0]["id"] == "x-done1"
    assert board["in_progress"]["rows"][0]["id"] == "x-prog1"
    assert "checks red" in board["in_progress"]["rows"][0]["fact"]
    assert board["on_deck"]["rows"][0]["id"] == "x-deck1"


def test_zero_subprocess_calls_through_the_cli(raising_subprocess, cache_dir, tmp_path, monkeypatch):
    """Same proof, but through the actual CLI command (parsing + compute +
    print), with an explicit --project so detect_project's local-git call
    (a different, non-network subprocess use) never enters the picture."""
    graph_path = tmp_path / "graph.json"
    graph_path.write_text(
        json.dumps({"entries": [_node("x-deck2", title="on deck via cli")]}), encoding="utf-8"
    )
    monkeypatch.setattr("fno.graph.cli._graph_path", lambda: graph_path)

    result = runner.invoke(cli, ["board", "--project", "fno", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["on_deck"]["rows"][0]["id"] == "x-deck2"


def test_board_module_imports_no_live_status_paths():
    """Second, cheaper guard: catch the regression at edit time. board.py must
    never import the live-read paths, only the offline cache reader."""
    src = inspect.getsource(board_module)
    tree = ast.parse(src)
    imported_modules = set()
    imported_names = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                imported_modules.add(alias.name)
        elif isinstance(node, ast.ImportFrom):
            if node.module:
                imported_modules.add(node.module)
                for alias in node.names:
                    imported_names.add(f"{node.module}.{alias.name}")

    assert "fno.pr._status" not in imported_modules
    assert "fno.pr._rest" not in imported_modules
    assert "fno.pr._cache.cached_status" not in imported_names


def test_a_stray_shellout_on_the_board_path_fails_loudly(raising_subprocess, cache_dir, monkeypatch):
    """Given an implementation that reaches subprocess.run anywhere on the
    board's data path (standing in for a mistaken cached_status/live call),
    when the test runs, then it fails with the raised error rather than
    passing quietly - proof the instrument is wired, not merely that the
    real implementation happens to have zero calls today."""

    def _rogue_offline(slug_key, pr):
        subprocess.run(["true"])  # any accidental shell-out, caught live
        return None

    monkeypatch.setattr(board_module, "newest_row_offline", _rogue_offline)
    entries = [
        _node(
            "x-prog-rogue", status="in_progress", pr_number=1,
            pr_url="https://github.com/acme/widgets/pull/1",
        )
    ]

    with pytest.raises(AssertionError, match="must never shell out"):
        compute_board(entries, project="fno", now=NOW)


# ---------------------------------------------------------------------------
# Section membership + unknown-vs-empty
# ---------------------------------------------------------------------------


def test_live_claimed_node_in_progress_not_also_on_deck(monkeypatch, cache_dir):
    monkeypatch.setattr(board_module, "live_claimed_node_ids", lambda **_kw: {"x-claimed"})
    entries = [
        _node("x-claimed", title="claimed by a live session"),
        _node("x-ready1"),
        _node("x-ready2"),
        _node("x-ready3"),
    ]

    board = compute_board(entries, project="fno", now=NOW)

    ip_ids = {r["id"] for r in board["in_progress"]["rows"]}
    deck_ids = {r["id"] for r in board["on_deck"]["rows"]}
    assert "x-claimed" in ip_ids
    assert "x-claimed" not in deck_ids


def test_in_progress_epic_lands_in_progress_not_on_deck(cache_dir):
    """An epic never carries its own claim or status=="in_progress" -
    sessions claim leaf children, not containers - so it needs the separate
    in_progress_epics promotion to avoid falling through to On deck."""
    entries = [
        _node("x-epic1", type="epic", priority="p1"),
        _node("x-child1", parent="x-epic1", status="in_progress"),
    ]

    board = compute_board(entries, project="fno", now=NOW)

    ip_ids = {r["id"] for r in board["in_progress"]["rows"]}
    deck_ids = {r["id"] for r in board["on_deck"]["rows"]}
    assert "x-epic1" in ip_ids
    assert "x-epic1" not in deck_ids


def test_no_completions_in_window_falls_back_to_most_recent(cache_dir):
    entries = [
        _node("x-old-done", completed_at=_iso(NOW - timedelta(days=10)), pr_number=1),
    ]

    board = compute_board(entries, project="fno", now=NOW)

    assert board["just_finished"]["rows"][0]["id"] == "x-old-done"
    assert board["just_finished"]["note"]


def test_nothing_completed_reads_none_and_is_not_unknown(cache_dir):
    entries = [_node("x-ready1")]

    board = compute_board(entries, project="fno", now=NOW)

    assert board["just_finished"]["rows"] == []
    assert board["just_finished"]["unknown"] is None


def test_more_than_five_in_a_section_caps_and_counts(cache_dir):
    entries = [_node(f"x-deck{i}") for i in range(7)]

    board = compute_board(entries, project="fno", now=NOW)

    assert len(board["on_deck"]["rows"]) == 5
    assert board["on_deck"]["more"] == 2


def test_claims_fault_degrades_in_progress_to_unknown_not_empty(monkeypatch, cache_dir):
    def _raise(**_kw):
        raise RuntimeError("claims root unreadable")

    monkeypatch.setattr(board_module, "live_claimed_node_ids", _raise)
    entries = [
        _node("x-done1", completed_at=_iso(NOW - timedelta(hours=1)), pr_number=5),
        _node("x-ready1"),
    ]

    board = compute_board(entries, project="fno", now=NOW)

    assert board["in_progress"]["unknown"]
    assert board["in_progress"]["rows"] == []
    # A claims fault must not also blank out an unrelated section.
    assert board["just_finished"]["rows"][0]["id"] == "x-done1"


def test_claims_fault_does_not_leak_a_status_in_progress_node_to_on_deck(monkeypatch, cache_dir):
    """A node the graph itself marks in_progress must stay out of On deck
    even when the live-claims read fails (in_progress_ids is empty then,
    but status alone is still enough to exclude it)."""

    def _raise(**_kw):
        raise RuntimeError("claims root unreadable")

    monkeypatch.setattr(board_module, "live_claimed_node_ids", _raise)
    entries = [_node("x-prog1", status="in_progress")]

    board = compute_board(entries, project="fno", now=NOW)

    assert board["on_deck"]["rows"] == []


def test_graph_unreadable_renders_unknown_and_exits_nonzero(tmp_path, monkeypatch):
    graph_path = tmp_path / "graph.json"
    graph_path.write_text("not json", encoding="utf-8")
    monkeypatch.setattr("fno.graph.cli._graph_path", lambda: graph_path)

    result = runner.invoke(cli, ["board", "--project", "fno", "--json"])

    assert result.exit_code == 1
    payload = json.loads(result.output)
    for section in payload.values():
        assert section["unknown"]


# ---------------------------------------------------------------------------
# Per-line facts + word cap
# ---------------------------------------------------------------------------


def test_on_deck_names_first_unmet_blocker(cache_dir):
    entries = [
        _node("x-blocker", completed_at=None),
        _node("x-blocked", blocked_by=["x-blocker"]),
    ]

    board = compute_board(entries, project="fno", now=NOW)

    blocked_row = next(r for r in board["on_deck"]["rows"] if r["id"] == "x-blocked")
    assert blocked_row["fact"] == "blocked by x-blocker"


def test_on_deck_ready_when_no_unmet_blocker(cache_dir):
    entries = [_node("x-solo")]

    board = compute_board(entries, project="fno", now=NOW)

    assert board["on_deck"]["rows"][0]["fact"] == "ready"


def test_in_progress_no_pr_yet(cache_dir):
    entries = [_node("x-prog-nopr", status="in_progress")]

    board = compute_board(entries, project="fno", now=NOW)

    assert board["in_progress"]["rows"][0]["fact"] == "no PR yet"


def test_in_progress_pr_with_no_cached_row_reads_unknown(cache_dir):
    entries = [
        _node(
            "x-prog-nocache", status="in_progress", pr_number=42,
            pr_url="https://github.com/acme/widgets/pull/42",
        )
    ]

    board = compute_board(entries, project="fno", now=NOW)

    assert board["in_progress"]["rows"][0]["fact"] == "PR 42 unknown (no cached row)"


def test_long_title_is_capped_but_fact_survives(cache_dir):
    long_title = " ".join(f"word{i}" for i in range(20))
    entries = [_node("x-deck-long", title=long_title, blocked_by=["x-missing-blocker"])]

    board = compute_board(entries, project="fno", now=NOW)

    row = board["on_deck"]["rows"][0]
    assert len(row["title"].rstrip("…").split()) == 12
    assert row["fact"] == "ready"  # blocker not in graph -> not unmet
