"""Tests for the work-item tracker seam (bring-your-own-id foundation).

Covers the five-field read projection, the single close write, the footnote-
owned sidecar roundtrip, and the backend factory. The partition invariant
itself (zero overlap between sidecar and read interface) has its own CI gate
in scripts/ci/check-tracker-partition.sh, exercised in test_partition_gate.py.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths import sidecar_path
from fno.tracker import GraphTracker, NodeNotFound, TrackerState, get_tracker
from fno.tracker import sidecar as sidecar_mod
from fno.tracker.sidecar import Sidecar, load, save


def _write_graph(path: Path, entries: list[dict]) -> Path:
    path.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    return path


def test_read_projects_five_fields(tmp_path):
    g = _write_graph(
        tmp_path / "graph.json",
        [{"id": "ab-deadbeef", "title": "Fix login", "plan_path": "/p.md"}],
    )
    node = GraphTracker(path=g).read("ab-deadbeef")
    # Exactly the five-field interface, nothing more.
    assert node.id == "ab-deadbeef"
    assert node.title == "Fix login"
    assert node.state is TrackerState.open
    assert node.parent is None
    assert node.blocked_by == []
    assert set(node.model_fields) == {"id", "title", "state", "parent", "blocked_by"}


def test_read_missing_raises(tmp_path):
    g = _write_graph(tmp_path / "graph.json", [{"id": "ab-deadbeef"}])
    with pytest.raises(NodeNotFound):
        GraphTracker(path=g).read("ab-missing")


def test_state_closed_only_for_terminal_rungs(tmp_path):
    g = _write_graph(
        tmp_path / "graph.json",
        [
            {"id": "ab-done", "completed_at": "2026-01-01T00:00:00Z"},
            {"id": "ab-sup", "superseded_by": "ab-other"},
            {"id": "ab-open", "plan_path": "/p.md"},
        ],
    )
    t = GraphTracker(path=g)
    assert t.read("ab-done").state is TrackerState.closed
    assert t.read("ab-sup").state is TrackerState.closed
    assert t.read("ab-open").state is TrackerState.open


def test_close_sets_completed_and_flips_state(tmp_path):
    g = _write_graph(
        tmp_path / "graph.json",
        [{"id": "ab-deadbeef", "plan_path": "/p.md"}],
    )
    t = GraphTracker(path=g)
    assert t.read("ab-deadbeef").state is TrackerState.open
    t.close("ab-deadbeef")
    # close sets completed_at; the store derives status=done, which projects to
    # closed on the next read. The point of the test: close is observable via
    # the read interface, not by poking at stored fields.
    assert t.read("ab-deadbeef").state is TrackerState.closed


def test_close_missing_raises(tmp_path):
    g = _write_graph(tmp_path / "graph.json", [{"id": "ab-deadbeef"}])
    with pytest.raises(NodeNotFound):
        GraphTracker(path=g).close("ab-missing")


def test_graph_tracker_satisfies_protocol():
    # runtime_checkable: GraphTracker is structurally a NodeTracker.
    from fno.tracker.types import NodeTracker

    assert isinstance(GraphTracker(path=Path("/nonexistent")), NodeTracker)


def test_get_tracker_default_is_graph():
    t = get_tracker()
    assert t.name == "graph"
    assert isinstance(t, GraphTracker)


def test_get_tracker_unknown_backend():
    with pytest.raises(ValueError):
        get_tracker("linear")  # not shipped in the foundation


def test_sidecar_roundtrip(tmp_path, monkeypatch):
    monkeypatch.setattr(sidecar_mod, "sidecar_path", lambda i: tmp_path / f"{i}.json")
    sc = Sidecar(id="ENG-441", cwd="/repo", plan_path="/plan.md", pr_number=7)
    save_path = save(sc)
    loaded = load("ENG-441")
    assert loaded.cwd == "/repo"
    assert loaded.plan_path == "/plan.md"
    assert loaded.pr_number == 7
    assert save_path.exists()


def test_sidecar_path_encodes_separators(monkeypatch, tmp_path):
    # owner/repo#123 contains a path separator and must land as one filename,
    # reusing the claims key encoder. A positive assertion on the encoded name,
    # not an absence: the encoded name contains no raw '/' or '#'.
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    name = sidecar_path("owner/repo#123").name
    assert "/" not in name
    assert "#" not in name
    assert name.endswith(".json")
    assert sidecar_path("owner/repo#123").parent == tmp_path / "sidecar"
