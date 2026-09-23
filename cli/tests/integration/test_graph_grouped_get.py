"""Acceptance tests for the grouped human node view and residue migration."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.graph.grouped import (
    GROUPED_ASSIGNED_FIELDS,
    GROUPED_FIELD_GROUPS,
    GROUPED_RESIDUAL_FIELDS,
    GROUPED_SCHEMA_FIELDS,
)


runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    graph = tmp_path / "graph.json"
    graph.write_text('{"entries": []}\n')
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


def test_grouped_view_is_opt_in_and_preserves_flat_default(tmp_graph):
    tmp_graph.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "x-851b",
                        "title": "Grouped node",
                        "project": "fno",
                        "source": "origin-node",
                        "details": "human details",
                    }
                ]
            }
        )
        + "\n"
    )

    flat = runner.invoke(app, ["backlog", "get", "x-851b"])
    grouped = runner.invoke(app, ["backlog", "get", "x-851b", "--grouped"])

    assert flat.exit_code == 0, flat.output
    assert grouped.exit_code == 0, grouped.output
    assert json.loads(flat.output)["id"] == "x-851b"
    assert "Identity" in grouped.output
    assert "Provenance" in grouped.output
    assert "source (origin): origin-node" in grouped.output
    assert "human details" in grouped.output


def test_grouped_schema_fields_are_explicitly_assigned():
    grouped_fields = [field for _, fields in GROUPED_FIELD_GROUPS for field in fields]

    assert len(grouped_fields) == len(set(grouped_fields))
    assert not set(grouped_fields) & GROUPED_RESIDUAL_FIELDS
    assert not (GROUPED_SCHEMA_FIELDS - GROUPED_ASSIGNED_FIELDS), sorted(
        GROUPED_SCHEMA_FIELDS - GROUPED_ASSIGNED_FIELDS
    )


def test_unknown_populated_field_is_visible_in_residual(tmp_graph):
    tmp_graph.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "x-851b",
                        "future_field": "keep visible",
                        "reverted": False,
                        "points": 0,
                        "tags": [],
                    }
                ]
            }
        )
        + "\n"
    )

    result = runner.invoke(app, ["backlog", "get", "x-851b", "--grouped"])

    assert result.exit_code == 0, result.output
    assert "Residual" in result.output
    assert "future_field: keep visible" in result.output
    assert "reverted: false" in result.output
    assert "points: 0" in result.output
    assert "tags:" not in result.output


def test_updated_at_migration_is_idempotent_and_preserves_other_fields(tmp_graph):
    original = {
        "id": "ab-6d4a9b79",
        "title": "Residue",
        "__updated_at": "2026-05-06T19:07:37",
        "details": "keep me",
    }
    control = {"id": "x-control", "title": "Control", "details": "untouched"}
    tmp_graph.write_text(json.dumps({"entries": [original, control]}) + "\n")

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
