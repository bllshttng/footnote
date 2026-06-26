"""Integration tests for slug resolution + display in the backlog CLI (ab-f82e8083).

Covers: `get` by slug / bare-hex, `find` high-recall + handle-leading output +
slug in JSON, `ready` slug-leading rows, and the idempotent `backfill-slugs` verb.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


# -- get by slug / bare-hex --------------------------------------------------


def test_get_by_slug_resolves_to_node(tmp_graph):
    # AC1-HP: `get <slug>` resolves to the node's ab-id.
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno"},
    ])
    result = runner.invoke(app, ["backlog", "get", "dashless-spawn"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.stdout)
    assert data["id"] == "ab-994222ee"
    assert data["slug"] == "dashless-spawn"


def test_get_by_bare_hex_reprefixes(tmp_graph):
    # AC4-HP: `get 1234abcd` (no ab-, no hyphen) re-prefixes and resolves.
    _seed(tmp_graph, [
        {"id": "ab-1234abcd", "title": "Billing", "slug": "billing",
         "_status": "ready", "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "get", "1234abcd"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["id"] == "ab-1234abcd"


def test_get_unknown_target_exits_1(tmp_graph):
    # AC1-FR-ish: a target that matches no id/slug/bare-hex fails loud.
    _seed(tmp_graph, [
        {"id": "ab-aaaaaaaa", "title": "Thing", "slug": "thing",
         "_status": "ready", "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "get", "nonsense-slug"])
    assert result.exit_code == 1
    combined = result.stdout + (result.stderr or "")
    assert "nonsense-slug" in combined


def test_get_field_works_with_slug_input(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno"},
    ])
    result = runner.invoke(app, ["backlog", "get", "dashless-spawn", "--field", "id"])
    assert result.exit_code == 0, result.output
    assert result.stdout.strip() == "ab-994222ee"


# -- find: high recall + handle display --------------------------------------


def test_find_matches_details_high_recall(tmp_graph):
    # AC2-HP recall: the search term lives only in details, not the title.
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "mobile node-id entry", "slug": "mobile-entry",
         "details": "iOS autocorrect mangles ab- prefixes on a phone",
         "_status": "ready", "domain": "code", "project": "p"},
        {"id": "ab-bbbbbbbb", "title": "unrelated", "slug": "unrelated",
         "_status": "ready", "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "find", "ios autocorrect"])
    assert result.exit_code == 0, result.output
    assert "ab-994222ee" in result.stdout
    assert "ab-bbbbbbbb" not in result.stdout


def test_find_human_output_leads_with_handle(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno"},
    ])
    result = runner.invoke(app, ["backlog", "find", "dashless"])
    assert result.exit_code == 0, result.output
    # The row leads with `slug (ab-id)`.
    assert "dashless-spawn (ab-994222ee)" in result.stdout


def test_find_resolves_ab_prefixed_slug(tmp_graph):
    # codex P2: a slug that itself starts with `ab-` must resolve via find, the
    # same node `get` resolves - it must not be mis-routed to the id path.
    _seed(tmp_graph, [
        {"id": "ab-77777777", "title": "AB test cleanup", "slug": "ab-test-cleanup",
         "_status": "ready", "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "find", "ab-test-cleanup"])
    assert result.exit_code == 0, result.output
    assert "ab-77777777" in result.stdout
    assert "ab-test-cleanup (ab-77777777)" in result.stdout


def test_find_json_includes_slug(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno"},
    ])
    result = runner.invoke(app, ["backlog", "find", "dashless", "--json"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.stdout)
    assert data[0]["slug"] == "dashless-spawn"


# -- ready: slug leads -------------------------------------------------------


def test_ready_rows_lead_with_slug(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno", "plan_path": "p.md"},
    ])
    result = runner.invoke(app, ["backlog", "ready", "--all"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.stdout)
    assert data[0]["slug"] == "dashless-spawn"


# -- backfill-slugs ----------------------------------------------------------


def test_backfill_slugs_assigns_and_is_idempotent(tmp_graph):
    # AC5-EDGE: legacy nodes (no slug) get one; re-running changes nothing.
    _seed(tmp_graph, [
        {"id": "ab-aaaaaaaa", "title": "First thing", "_status": "ready",
         "domain": "code", "project": "p"},
        {"id": "ab-bbbbbbbb", "title": "Second thing", "_status": "ready",
         "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "backfill-slugs"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["slugs_assigned"] == 2
    by_id = {e["id"]: e for e in _read(tmp_graph)}
    assert by_id["ab-aaaaaaaa"]["slug"] == "first-thing"
    assert by_id["ab-bbbbbbbb"]["slug"] == "second-thing"

    # Re-run is a no-op.
    result2 = runner.invoke(app, ["backlog", "backfill-slugs"])
    assert result2.exit_code == 0, result2.output
    assert json.loads(result2.stdout)["slugs_assigned"] == 0


# -- update --details --------------------------------------------------------


def test_update_details_sets_and_clears(tmp_graph):
    # `update --details` edits rationale in place (no recreate-via-idea dupe).
    _seed(tmp_graph, [
        {"id": "ab-deadbeef", "title": "Thing", "slug": "thing", "_status": "ready",
         "domain": "code", "project": "p", "details": None},
    ])
    result = runner.invoke(app, ["backlog", "update", "ab-deadbeef", "--details", "the full rationale"])
    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0]["details"] == "the full rationale"

    # `null` clears it; --description is an accepted alias.
    result = runner.invoke(app, ["backlog", "update", "ab-deadbeef", "--description", "null"])
    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0]["details"] is None


def test_update_domain_size_type(tmp_graph):
    # Create-only fields are now editable, so a mistake never forces a recreate.
    _seed(tmp_graph, [
        {"id": "ab-feedface", "title": "Thing", "slug": "thing", "_status": "ready",
         "domain": "code", "type": "feature", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "update", "ab-feedface",
                                 "--domain", "design", "--size", "l", "--type", "epic"])
    assert result.exit_code == 0, result.output
    node = _read(tmp_graph)[0]
    assert node["domain"] == "design"
    assert node["size"] == "L"  # normalized to uppercase
    assert node["type"] == "epic"
