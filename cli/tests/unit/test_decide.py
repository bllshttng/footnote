"""Tests for `fno decide` - the durable decision record and its recovery query.

The record has two halves that must both exist: the append-only
``operator_decision`` event (survives compaction) and the projection onto the
subject node's graph entry (findable by subject, inherits the archive
read-through). A record that is only greppable is not recoverable.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.decide.cli import decide_app

runner = CliRunner()


def _node(nid: str, **over) -> dict:
    base = {
        "id": nid,
        "title": f"node {nid}",
        "status": "ready",
        "type": "feature",
        "priority": "p2",
    }
    base.update(over)
    return base


@pytest.fixture
def tmp_graph(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [_node("x-7d94")]}, indent=2) + "\n")
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # entries_with_archive resolves the archive through fno.paths, which is
    # graph_json().parent / "graph-archive.json"; pin both so the read-through
    # test stays hermetic.
    monkeypatch.setattr(
        "fno.paths.graph_archive_json", lambda: tmp_path / "graph-archive.json"
    )
    return g


@pytest.fixture
def root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A hermetic canonical root for the events journal (FNO_REPO_ROOT hook)."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    import fno.paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    return tmp_path


def _events(root: Path) -> list[dict]:
    return [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def test_record_appends_the_event_and_projects_onto_the_node(root: Path, tmp_graph: Path):
    """AC4-HP: fno decide writes the event AND the graph projection."""
    res = runner.invoke(
        decide_app,
        [
            "--subject", "x-7d94",
            "--decision", "fold every project's inbox",
            "--rationale", "a fold is a read; you do not migrate before you can see",
        ],
    )
    assert res.exit_code == 0, res.output
    did = res.stdout.strip().splitlines()[-1]
    assert did.startswith("d-"), "stdout carries the new decision id"

    events = [e for e in _events(root) if e["type"] == "operator_decision"]
    assert len(events) == 1
    data = events[0]["data"]
    assert data["decision_id"] == did
    assert data["subject"] == "x-7d94"
    assert data["decision"] == "fold every project's inbox"
    assert data["authority_source"]

    entry = json.loads(tmp_graph.read_text())["entries"][0]
    assert [d["decision_id"] for d in entry["decisions"]] == [did]
    assert entry["decisions"][0]["rationale"].startswith("a fold is a read")
    assert entry["decisions"][0]["superseded_by"] is None


def test_list_returns_decisions_newest_first(root: Path, tmp_graph: Path):
    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "first"])
    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "second"])

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert listed.output.index("second") < listed.output.index("first")

    as_json = runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"])
    payload = json.loads(as_json.stdout)
    assert [d["decision"] for d in payload["decisions"]] == ["second", "first"]


def test_supersession_marks_the_older_decision(root: Path, tmp_graph: Path):
    """AC6-EDGE: two decisions on one subject order themselves; the older one
    is marked, not hidden."""
    first = runner.invoke(
        decide_app, ["--subject", "x-7d94", "--decision", "migrate now"]
    ).stdout.strip().splitlines()[-1]
    second = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "fold first", "--supersedes", first],
    )
    assert second.exit_code == 0, second.output

    entry = json.loads(tmp_graph.read_text())["entries"][0]
    by_id = {d["decision_id"]: d for d in entry["decisions"]}
    assert by_id[first]["superseded_by"] is not None
    assert by_id[first]["superseded_by"].startswith("d-")

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert "superseded by" in listed.output, "the render marks the superseded row"


def test_list_survives_archiving_of_the_subject(root: Path, tmp_graph: Path):
    """AC5-EDGE: a decision recorded pre-archive is still listable post-archive
    through entries_with_archive."""
    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "fold first"])
    entries = json.loads(tmp_graph.read_text())["entries"]
    archive = tmp_graph.parent / "graph-archive.json"
    archive.write_text(json.dumps({"entries": entries}) + "\n")
    tmp_graph.write_text(json.dumps({"entries": []}) + "\n")

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert "fold first" in listed.output


def test_list_refuses_a_subject_that_resolves_to_nothing(root: Path, tmp_graph: Path):
    listed = runner.invoke(decide_app, ["list", "--subject", "x-nope"])
    assert listed.exit_code == 1


def test_record_without_a_resolvable_subject_still_writes_the_event(
    root: Path, tmp_graph: Path
):
    """A subject that names a file or area, not a node, loses the projection
    but keeps the durable event; the verb says so on stderr."""
    res = runner.invoke(
        decide_app, ["--subject", "docs/architecture.md", "--decision", "keep it"]
    )
    assert res.exit_code == 0, res.output
    events = [e for e in _events(root) if e["type"] == "operator_decision"]
    assert len(events) == 1
    assert events[0]["data"]["subject"] == "docs/architecture.md"
    assert "no node" in res.output.lower() or "projection" in res.output.lower()


def test_decisions_default_applies_on_read_for_legacy_rows(tmp_path: Path):
    """The decisions default lives in _apply_graph_defaults, the one migration
    seam: a pre-decision graph row reads [] without a rewrite."""
    from fno.graph.store import _apply_graph_defaults

    entries = _apply_graph_defaults([{"id": "x-old", "title": "old", "status": "ready"}])
    assert entries[0]["decisions"] == []
