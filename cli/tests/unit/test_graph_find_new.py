"""Unit tests for `fno find` and `fno new` commands.

find: fuzzy search across graph entries with optional filters.
new:  append a new ab- entry without a plan file (for non-code tasks).

Uses typer.testing.CliRunner with monkey-patched GRAPH_JSON.
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
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # Seam readers (guarded metadata/display reads) resolve paths.graph_json
    # at call time; pin the resolver to the same hermetic file.
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    """Rows the way the store writes them: the typed api drops a row the
    model cannot parse, so seeds carry the stamped fields."""
    complete = []
    for e in entries:
        row = {"type": "feature", "priority": "p2", "status": "idea", **e}
        row.setdefault("title", e.get("id", "node"))
        row.setdefault("slug", e.get("id", "node"))
        if row["status"] == "done" and not row.get("completed_at"):
            row["completed_at"] = "2026-09-01T00:00:00Z"
        complete.append(row)
    g.write_text(json.dumps({"entries": complete}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    from fno.graph.store import read_graph_strict

    return read_graph_strict(g)


# -- new --


def test_scenario4_hp_new_creates_entry(tmp_graph):
    """Scenario 4 (HP): fno new writes a new ab- entry with defaults."""
    result = runner.invoke(
        app,
        ["backlog", "new", "Research: Q3 budgets", "--domain", "research"],
    )
    assert result.exit_code == 0, result.output
    # Emits the new id on stdout
    stdout = result.stdout.strip()
    assert stdout.startswith("ab-"), stdout
    new_id = next(line for line in stdout.splitlines() if line.startswith("ab-"))
    entries = _read(tmp_graph)
    assert len(entries) == 1
    e = entries[0]
    assert e["id"] == new_id
    assert e["title"] == "Research: Q3 budgets"
    assert e["domain"] == "research"
    # `fno new` creates plan-less nodes, so they derive to idea (not ready).
    # The `ready` state requires a plan_path.
    assert e["status"] == "idea"
    assert e["source"] == "fno-new"


def test_new_default_domain_is_code(tmp_graph):
    """Without --domain, new entries default to code."""
    result = runner.invoke(app, ["backlog", "new", "Some code task"])
    assert result.exit_code == 0, result.output
    e = _read(tmp_graph)[0]
    assert e["domain"] == "code"


def test_new_sets_project_and_priority(tmp_graph):
    """--project and --priority flow through."""
    result = runner.invoke(
        app,
        [
            "backlog", "new", "Urgent thing",
            "--project", "acme",
            "--priority", "p1",
        ],
    )
    assert result.exit_code == 0, result.output
    e = _read(tmp_graph)[0]
    assert e["project"] == "acme"
    assert e["priority"] == "p1"


def test_scenario5_edge_new_fuzzy_domain_warns(tmp_graph):
    """Scenario 5 (EDGE): fuzzy domain match asks for --force-domain."""
    _seed(tmp_graph, [
        {"id": "ab-seed0001", "title": "seed", "status": "done",
         "domain": "research", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "new", "New task", "--domain", "res"])
    assert result.exit_code == 2, result.output
    combined = result.stdout + (result.stderr or "")
    assert "research" in combined
    assert "force-domain" in combined.lower() or "--force-domain" in combined
    # No entry written
    entries = _read(tmp_graph)
    assert len(entries) == 1  # only the seed
    assert entries[0]["id"] == "ab-seed0001"


def test_scenario6_edge_new_force_domain_bypasses(tmp_graph):
    """Scenario 6 (EDGE): --force-domain bypasses the suggestion."""
    _seed(tmp_graph, [
        {"id": "ab-seed0001", "title": "seed", "status": "done",
         "domain": "research", "project": "p"},
    ])
    result = runner.invoke(
        app, ["backlog", "new", "New task", "--domain", "res", "--force-domain"],
    )
    assert result.exit_code == 0, result.output
    # New entry has domain="res" verbatim (not auto-corrected to research)
    entries = _read(tmp_graph)
    new_entries = [e for e in entries if e["id"] != "ab-seed0001"]
    assert len(new_entries) == 1
    assert new_entries[0]["domain"] == "res"


def test_new_exact_domain_match_no_warning(tmp_graph):
    """Exact domain match (confidence=exact) does NOT trigger warning."""
    _seed(tmp_graph, [
        {"id": "ab-seed0001", "title": "seed", "status": "done",
         "domain": "research", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "new", "New task", "--domain", "research"])
    assert result.exit_code == 0, result.output


def test_new_unfamiliar_domain_passes_through(tmp_graph):
    """Truly new domain (confidence=new, no prefix collision) passes through."""
    _seed(tmp_graph, [
        {"id": "ab-seed0001", "title": "seed", "status": "done",
         "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "new", "New task", "--domain", "trading"])
    assert result.exit_code == 0, result.output
    entries = _read(tmp_graph)
    new_entries = [e for e in entries if e["id"] != "ab-seed0001"]
    assert new_entries[0]["domain"] == "trading"


def test_new_id_has_correct_prefix_and_length(tmp_graph):
    """Generated id matches the ab-xxxxxxxx pattern."""
    result = runner.invoke(app, ["backlog", "new", "T"])
    assert result.exit_code == 0
    e = _read(tmp_graph)[0]
    assert e["id"].startswith("ab-")
    assert len(e["id"]) == 11  # ab- + 8 hex chars


def test_new_sets_created_at_iso8601(tmp_graph):
    """created_at is ISO 8601."""
    result = runner.invoke(app, ["backlog", "new", "T"])
    assert result.exit_code == 0
    e = _read(tmp_graph)[0]
    assert e["created_at"]
    assert "T" in e["created_at"]


# -- top-level alias sanity --


def test_new_under_graph_also_works(tmp_graph):
    """`fno backlog new ...` alias works the same as `fno new`."""
    result = runner.invoke(app, ["backlog", "new", "Via graph app"])
    assert result.exit_code == 0, result.output
    assert result.stdout.strip().startswith("ab-")


# -- Task 1.2: --source-* flags on fno new --


def test_ac1_hp_new_with_source_flags_populates_provenance(tmp_graph):
    """AC1-HP: fno new --source-kind from_inbox creates entry with all four source fields."""
    result = runner.invoke(
        app,
        [
            "backlog", "new", "Add region filter",
            "--project", "acme-web",
            "--source-kind", "from_inbox",
            "--source-project", "example-pipeline",
            "--source-inbox-msg", "msg-a4f1",
        ],
    )
    assert result.exit_code == 0, result.output
    entries = _read(tmp_graph)
    assert len(entries) == 1
    e = entries[0]
    assert e["source_kind"] == "from_inbox"
    assert e["source_project"] == "example-pipeline"
    assert e["source_inbox_msg"] == "msg-a4f1"
    # source_session_id not provided, should be None
    assert e.get("source_session_id") is None


def test_ac1_hp_new_with_source_session_id(tmp_graph):
    """AC1-HP: --source-session-id is also stored on the entry."""
    result = runner.invoke(
        app,
        [
            "backlog", "new", "Session sourced task",
            "--source-kind", "from_supervisor",
            "--source-session-id", "sess-xyz123",
        ],
    )
    assert result.exit_code == 0, result.output
    e = _read(tmp_graph)[0]
    assert e["source_kind"] == "from_supervisor"
    assert e["source_session_id"] == "sess-xyz123"


def test_ac2_err_new_invalid_source_kind_rejected(tmp_graph):
    """AC2-ERR: --source-kind invalid_value exits non-zero, no graph mutation."""
    result = runner.invoke(
        app,
        ["backlog", "new", "x", "--source-kind", "invalid_value"],
    )
    assert result.exit_code != 0, result.output
    # No entries written
    entries = _read(tmp_graph)
    assert len(entries) == 0


def test_new_source_kind_defaults_to_organic(tmp_graph):
    """Without --source-kind, new entries default to source_kind=organic."""
    result = runner.invoke(app, ["backlog", "new", "Organic task"])
    assert result.exit_code == 0, result.output
    e = _read(tmp_graph)[0]
    assert e["source_kind"] == "organic"
    assert e.get("source_project") is None
    assert e.get("source_session_id") is None
    assert e.get("source_inbox_msg") is None
