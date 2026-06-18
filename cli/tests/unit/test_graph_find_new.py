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
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


# -- find --


def test_scenario1_hp_find_single_match(tmp_graph):
    """Scenario 1 (HP): single fuzzy match prints one line."""
    _seed(tmp_graph, [
        {"id": "ab-q2000001", "title": "Q2 outreach campaign", "_status": "ready",
         "domain": "research", "project": "acme"},
        {"id": "ab-other0001", "title": "Unrelated", "_status": "ready",
         "domain": "code", "project": "fno"},
    ])
    result = runner.invoke(app, ["find", "outreach"])
    assert result.exit_code == 0, result.output
    assert "ab-q2000001" in result.stdout
    assert "Q2 outreach campaign" in result.stdout
    # Single match -> one line
    nonempty = [ln for ln in result.stdout.splitlines() if ln.strip()]
    assert len(nonempty) == 1


def test_scenario2_hp_find_with_status_filter(tmp_graph):
    """Scenario 2 (HP): --status filter narrows results."""
    _seed(tmp_graph, [
        {"id": "ab-pl000001", "title": "Plan 01", "_status": "ready",
         "domain": "code", "project": "fno"},
        {"id": "ab-pl000002", "title": "Plan 02", "_status": "done",
         "domain": "code", "project": "fno"},
    ])
    result = runner.invoke(app, ["find", "plan", "--status", "ready"])
    assert result.exit_code == 0, result.output
    assert "ab-pl000001" in result.stdout
    assert "ab-pl000002" not in result.stdout


def test_find_with_project_filter(tmp_graph):
    """--project filter narrows by project."""
    _seed(tmp_graph, [
        {"id": "ab-aa000001", "title": "shared title", "_status": "ready",
         "domain": "code", "project": "fno"},
        {"id": "ab-aa000002", "title": "shared title", "_status": "ready",
         "domain": "code", "project": "another-project"},
    ])
    result = runner.invoke(app, ["find", "shared", "--project", "fno"])
    assert result.exit_code == 0, result.output
    assert "ab-aa000001" in result.stdout
    assert "ab-aa000002" not in result.stdout


def test_find_with_domain_filter(tmp_graph):
    """--domain filter narrows by domain."""
    _seed(tmp_graph, [
        {"id": "ab-aa000001", "title": "task", "_status": "ready",
         "domain": "research", "project": "p"},
        {"id": "ab-aa000002", "title": "task", "_status": "ready",
         "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["find", "task", "--domain", "research"])
    assert result.exit_code == 0
    assert "ab-aa000001" in result.stdout
    assert "ab-aa000002" not in result.stdout


def test_scenario3_hp_find_json_output(tmp_graph):
    """Scenario 3 (HP): --json output is parseable as a JSON array."""
    _seed(tmp_graph, [
        {"id": "ab-js000001", "title": "Q2 outreach",
         "_status": "ready", "domain": "research", "project": "p"},
    ])
    result = runner.invoke(app, ["find", "outreach", "--json"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.stdout)
    assert isinstance(data, list)
    assert data[0]["id"] == "ab-js000001"


def test_scenario7_err_find_no_matches(tmp_graph):
    """Scenario 7 (ERR): no matches exits 1 with a clear message."""
    _seed(tmp_graph, [
        {"id": "ab-aa000001", "title": "some title", "_status": "ready",
         "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["find", "xyzzy"])
    assert result.exit_code == 1
    combined = result.stdout + (result.stderr or "")
    assert "xyzzy" in combined
    assert "no match" in combined.lower()


def test_find_shows_multiple_matches(tmp_graph):
    """Multi-match: all candidates printed."""
    _seed(tmp_graph, [
        {"id": "ab-pp000001", "title": "Plan 01", "_status": "ready",
         "domain": "code", "project": "p"},
        {"id": "ab-pp000002", "title": "Plan 02", "_status": "ready",
         "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["find", "plan"])
    assert result.exit_code == 0
    assert "ab-pp000001" in result.stdout
    assert "ab-pp000002" in result.stdout


# -- new --


def test_scenario4_hp_new_creates_entry(tmp_graph):
    """Scenario 4 (HP): fno new writes a new ab- entry with defaults."""
    result = runner.invoke(
        app,
        ["new", "Research: Q3 budgets", "--domain", "research"],
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
    assert e["_status"] == "idea"
    assert e["source"] == "abi-new"


def test_new_default_domain_is_code(tmp_graph):
    """Without --domain, new entries default to code."""
    result = runner.invoke(app, ["new", "Some code task"])
    assert result.exit_code == 0, result.output
    e = _read(tmp_graph)[0]
    assert e["domain"] == "code"


def test_new_sets_project_and_priority(tmp_graph):
    """--project and --priority flow through."""
    result = runner.invoke(
        app,
        [
            "new", "Urgent thing",
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
        {"id": "ab-seed0001", "title": "seed", "_status": "done",
         "domain": "research", "project": "p"},
    ])
    result = runner.invoke(app, ["new", "New task", "--domain", "res"])
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
        {"id": "ab-seed0001", "title": "seed", "_status": "done",
         "domain": "research", "project": "p"},
    ])
    result = runner.invoke(
        app, ["new", "New task", "--domain", "res", "--force-domain"],
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
        {"id": "ab-seed0001", "title": "seed", "_status": "done",
         "domain": "research", "project": "p"},
    ])
    result = runner.invoke(app, ["new", "New task", "--domain", "research"])
    assert result.exit_code == 0, result.output


def test_new_unfamiliar_domain_passes_through(tmp_graph):
    """Truly new domain (confidence=new, no prefix collision) passes through."""
    _seed(tmp_graph, [
        {"id": "ab-seed0001", "title": "seed", "_status": "done",
         "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["new", "New task", "--domain", "trading"])
    assert result.exit_code == 0, result.output
    entries = _read(tmp_graph)
    new_entries = [e for e in entries if e["id"] != "ab-seed0001"]
    assert new_entries[0]["domain"] == "trading"


def test_new_id_has_correct_prefix_and_length(tmp_graph):
    """Generated id matches the ab-xxxxxxxx pattern."""
    result = runner.invoke(app, ["new", "T"])
    assert result.exit_code == 0
    e = _read(tmp_graph)[0]
    assert e["id"].startswith("ab-")
    assert len(e["id"]) == 11  # ab- + 8 hex chars


def test_new_sets_created_at_iso8601(tmp_graph):
    """created_at is ISO 8601."""
    result = runner.invoke(app, ["new", "T"])
    assert result.exit_code == 0
    e = _read(tmp_graph)[0]
    assert e["created_at"]
    assert "T" in e["created_at"]


# -- top-level alias sanity --


def test_find_under_graph_also_works(tmp_graph):
    """`fno backlog find ...` alias works the same as `fno find`."""
    _seed(tmp_graph, [
        {"id": "ab-fi000001", "title": "findable", "_status": "ready",
         "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "find", "findable"])
    assert result.exit_code == 0, result.output
    assert "ab-fi000001" in result.stdout


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
            "new", "Add region filter",
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
    assert e["source_session_id"] is None


def test_ac1_hp_new_with_source_session_id(tmp_graph):
    """AC1-HP: --source-session-id is also stored on the entry."""
    result = runner.invoke(
        app,
        [
            "new", "Session sourced task",
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
        ["new", "x", "--source-kind", "invalid_value"],
    )
    assert result.exit_code != 0, result.output
    # No entries written
    entries = _read(tmp_graph)
    assert len(entries) == 0


def test_new_source_kind_defaults_to_organic(tmp_graph):
    """Without --source-kind, new entries default to source_kind=organic."""
    result = runner.invoke(app, ["new", "Organic task"])
    assert result.exit_code == 0, result.output
    e = _read(tmp_graph)[0]
    assert e["source_kind"] == "organic"
    assert e["source_project"] is None
    assert e["source_session_id"] is None
    assert e["source_inbox_msg"] is None
