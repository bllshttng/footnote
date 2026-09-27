"""Acceptance tests for the residue migration. The grouped human node view
is the native binary's now (crates/fno-agents/src/backlog/render.rs pins its
bytes); what stays here is the `__updated_at` residue migration."""

from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app


runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    graph = tmp_path / "graph.json"
    seed_graph(graph, '{"entries": []}\n')
    import fno.graph._constants as constants
    import fno.graph.store as store

    monkeypatch.setattr(constants, "GRAPH_JSON", graph)
    monkeypatch.setattr(constants, "GRAPH_ARCHIVE_JSON", tmp_path / "archive.json")
    monkeypatch.setattr(store, "GRAPH_JSON", graph)
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    return graph


def _read_entries(graph: Path) -> list[dict]:
    # The store owns state; graph.json is a frozen export, so read-backs
    # come from store rows.
    from fno.graph.store import read_graph_strict

    return read_graph_strict(graph)


def test_updated_at_migration_is_idempotent_and_preserves_other_fields(tmp_graph):
    original = {
        "id": "ab-6d4a9b79",
        "title": "Residue",
        "__updated_at": "2026-05-06T19:07:37",
        "details": "keep me",
    }
    control = {"id": "x-control", "title": "Control", "details": "untouched"}
    seed_graph(tmp_graph, json.dumps({"entries": [original, control]}) + "\n")

    dry_run = runner.invoke(app, ["backlog", "migrate-updated-at"])
    assert dry_run.exit_code == 0, dry_run.output
    assert json.loads(dry_run.output)["candidate_count"] == 1

    applied = runner.invoke(app, ["backlog", "migrate-updated-at", "--apply"])
    assert applied.exit_code == 0, applied.output
    rows = {row["id"]: row for row in _read_entries(tmp_graph)}
    assert "__updated_at" not in rows["ab-6d4a9b79"]
    assert rows["ab-6d4a9b79"]["title"] == original["title"]
    assert rows["ab-6d4a9b79"]["details"] == original["details"]
    for key, value in control.items():
        assert rows["x-control"][key] == value

    # Idempotent: a second apply finds no residue left to migrate. (The
    # byte-identity check died with the json leg: the file is a frozen
    # export, so the honest idempotence marker is the zero-candidate
    # receipt.)
    second = runner.invoke(app, ["backlog", "migrate-updated-at", "--apply"])
    assert second.exit_code == 0, second.output
    receipt = json.loads(second.output)
    assert receipt["candidate_count"] == 0
    assert receipt["removed"] == 0
