"""Crowning surfaces an epic's settled children, never re-derives them (x-ada6).

The check-in body filters the board to open rows, so a freshly crowned king
walks past the done children that record what its epic already established and
re-measures them. `fno agents king init` is the crowning verb, so its output
names the settled children as titles. Nothing prints when none exist: an
absent section is the positive control, never an empty one.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.paths import graph_json
from fno.paths_testing import use_tmpdir


@pytest.fixture
def court(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    monkeypatch.setattr("fno.king.state.king_loop_enabled", lambda: True)
    return tmp_path


def _seed(rows: list[dict]) -> None:
    graph_json().write_text(json.dumps({"entries": rows}), encoding="utf-8")


def _init(court, scope: str):
    from fno.king.cli import king_app

    return CliRunner().invoke(
        king_app,
        ["init", "--scope", scope, "--harness-session-id", "sess-1"],
    )


def test_crowning_output_names_the_settled_children(court) -> None:
    _seed(
        [
            {"id": "epic-1", "type": "epic", "title": "the epic"},
            {
                "id": "kid-1",
                "parent": "epic-1",
                "status": "done",
                "title": "drain cost measured at 11 seconds per call",
            },
            {
                "id": "kid-2",
                "parent": "epic-1",
                "status": "superseded",
                "title": "megawalk dispatch, moved to compose",
            },
            {
                "id": "kid-3",
                "parent": "epic-1",
                "status": "ready",
                "title": "open work the board already shows",
            },
            {
                "id": "kid-4",
                "parent": "epic-2",
                "status": "done",
                "title": "another epic's settled row",
            },
        ]
    )

    result = _init(court, "epic-1")

    assert result.exit_code == 0, result.output
    assert "Settled findings" in result.output
    assert "drain cost measured at 11 seconds per call" in result.output
    assert "megawalk dispatch, moved to compose" in result.output
    assert "open work the board already shows" not in result.output
    assert "another epic's settled row" not in result.output


def test_no_settled_children_prints_no_section(court) -> None:
    _seed(
        [
            {"id": "epic-1", "type": "epic", "title": "the epic"},
            {
                "id": "kid-1",
                "parent": "epic-1",
                "status": "ready",
                "title": "open work",
            },
        ]
    )

    result = _init(court, "epic-1")

    assert result.exit_code == 0, result.output
    assert "Settled findings" not in result.output


def test_unreadable_graph_prints_no_section(court, monkeypatch) -> None:
    monkeypatch.setattr("fno.agents.crown._graph_index", lambda: None)

    result = _init(court, "epic-1")

    assert result.exit_code == 0, result.output
    assert "Settled findings" not in result.output
