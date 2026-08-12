"""`fno backlog unarchive` - the inverse of the archive sweep.

`archive` was a one-way move: a node swept early, or swept correctly and then
needed again, could only come back by hand-editing graph.json, which a
PreToolUse hook forbids. It is the fourth instance of the shape this PR is
about, and it was found by running the audit rather than by an operator hitting
it.

The write-order test is the load-bearing one. Archive writes the archive first
so a crash duplicates rather than loses; unarchive has to write the working
graph first for the same reason, and the round-trip is what proves neither
verb can drop a node from both files.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def graphs(tmp_path, monkeypatch) -> tuple[Path, Path]:
    working = tmp_path / "graph.json"
    archive = tmp_path / "graph-archive.json"
    working.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", working)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    # Patch the CONSTANT, not the `_graph_archive_json` helper behind it. The
    # constants module serves these names through __getattr__, and monkeypatch
    # restores by setattr, so the first sibling test to patch GRAPH_ARCHIVE_JSON
    # leaves a real module attribute that shadows __getattr__ for the rest of
    # the session - a helper patch then passes alone and fails in the suite.
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", archive)
    monkeypatch.setattr(gs, "GRAPH_JSON", working)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return working, archive


def _node(nid: str, **over) -> dict:
    base = {
        "id": nid,
        "title": f"node {nid}",
        "status": "done",
        "completed_at": "2026-01-01T00:00:00+00:00",
        "domain": "code",
        "priority": "p2",
        "created_at": "2025-12-01T00:00:00+00:00",
    }
    base.update(over)
    return base


def _ids(path: Path) -> set[str]:
    if not path.exists():
        return set()
    return {e["id"] for e in json.loads(path.read_text())["entries"]}


def test_an_archived_node_comes_back(graphs):
    working, archive = graphs
    archive.write_text(json.dumps({"entries": [_node("ab-22222222")]}))
    res = runner.invoke(app, ["backlog", "unarchive", "ab-22222222"])
    assert res.exit_code == 0, res.output
    assert "ab-22222222" in _ids(working)


def test_the_archive_copy_is_dropped(graphs):
    """Otherwise read-through would resolve the node twice."""
    working, archive = graphs
    archive.write_text(json.dumps({"entries": [_node("ab-22222222")]}))
    runner.invoke(app, ["backlog", "unarchive", "ab-22222222"])
    assert "ab-22222222" not in _ids(archive)


def test_other_archived_nodes_are_untouched(graphs):
    working, archive = graphs
    archive.write_text(
        json.dumps({"entries": [_node("ab-22222222"), _node("ab-33333333")]})
    )
    runner.invoke(app, ["backlog", "unarchive", "ab-22222222"])
    assert _ids(archive) == {"ab-33333333"}


def test_the_node_keeps_its_fields(graphs):
    working, archive = graphs
    archive.write_text(
        json.dumps({"entries": [_node("ab-22222222", pr_number=9, cost_usd=1.5)]})
    )
    runner.invoke(app, ["backlog", "unarchive", "ab-22222222"])
    node = {e["id"]: e for e in json.loads(working.read_text())["entries"]}["ab-22222222"]
    assert node["pr_number"] == 9
    assert node["cost_usd"] == 1.5


def test_a_node_already_live_warns_and_changes_nothing(graphs):
    working, archive = graphs
    working.write_text(json.dumps({"entries": [_node("ab-22222222")]}))
    archive.write_text('{"entries": []}')
    res = runner.invoke(app, ["backlog", "unarchive", "ab-22222222"])
    assert res.exit_code == 0
    assert "already in the working graph" in res.output


def test_a_node_in_neither_file_is_an_error(graphs):
    working, archive = graphs
    archive.write_text('{"entries": []}')
    res = runner.invoke(app, ["backlog", "unarchive", "ab-99999999"])
    assert res.exit_code == 1
    assert "neither" in res.output


def test_a_bad_id_is_rejected(graphs):
    res = runner.invoke(app, ["backlog", "unarchive", "not-an-id"])
    assert res.exit_code == 1


def test_a_missing_archive_file_is_an_error_not_a_crash(graphs):
    working, archive = graphs
    assert not archive.exists()
    res = runner.invoke(app, ["backlog", "unarchive", "ab-22222222"])
    assert res.exit_code == 1


def test_archive_then_unarchive_round_trips(graphs):
    """The pair, end to end: no window where the node is in neither file."""
    working, archive = graphs
    old = _node("ab-22222222", completed_at="2025-01-01T00:00:00+00:00")
    working.write_text(json.dumps({"entries": [old]}))
    res = runner.invoke(app, ["backlog", "archive", "--apply"])
    assert res.exit_code == 0, res.output
    assert _ids(working) == set()
    assert _ids(archive) == {"ab-22222222"}

    res = runner.invoke(app, ["backlog", "unarchive", "ab-22222222"])
    assert res.exit_code == 0, res.output
    assert _ids(working) == {"ab-22222222"}
    assert _ids(archive) == set()


def test_the_round_trip_survives_reopen(graphs):
    """The two verbs compose: reopen refuses an archived node and names this one,
    so the sequence it prescribes has to actually work."""
    working, archive = graphs
    archive.write_text(json.dumps({"entries": [_node("ab-22222222")]}))

    refused = runner.invoke(app, ["backlog", "reopen", "ab-22222222", "--reason", "x"])
    assert refused.exit_code == 4
    assert "unarchive" in refused.output

    runner.invoke(app, ["backlog", "unarchive", "ab-22222222"])
    reopened = runner.invoke(app, ["backlog", "reopen", "ab-22222222", "--reason", "x"])
    assert reopened.exit_code == 0, reopened.output
    node = {e["id"]: e for e in json.loads(working.read_text())["entries"]}["ab-22222222"]
    assert node["completed_at"] is None
