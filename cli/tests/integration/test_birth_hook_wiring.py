"""Integration: the v2 A1 birth paths route through on_node_born.

cmd_idea wiring lives in test_idea_think_spawn_wiring.py and the retro path in
test_retro_land.py. This covers the two remaining named A1 paths - decompose
child mint and intake - proving each invokes the shared hook for a real birth
and NOT for a non-birth (an idempotent re-decompose that only updates).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

import fno.graph._constants as gc
import fno.graph.store as gs
import fno.provenance.spawn_think as st
from fno.cli import app

runner = CliRunner()


def _invoke(*args, input=None):
    return runner.invoke(app, list(args), input=input, catch_exceptions=False)


def _route_graph(g: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)


@pytest.fixture
def capture_born(monkeypatch: pytest.MonkeyPatch):
    """Patch on_node_born; return the list of (node_id, run_state) it observed."""
    seen: list = []

    def fake(node, *, run_state=None, **k):
        seen.append(((node or {}).get("id"), run_state))

    monkeypatch.setattr(st, "on_node_born", fake)
    return seen


# -- decompose child mint --

_EPIC = {
    "id": "ab-epic0001", "parent": None, "title": "Epic", "type": "feature",
    "project": "fno", "cwd": "/tmp/proj", "priority": "p1", "domain": "code",
    "blocked_by": [], "plan_path": "internal/fno/plans/big.md#anchor",
    "created_at": "2026-01-01T00:00:00+00:00",
}
_GROUPS = json.dumps([
    {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []},
    {"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": ["1"]},
])


def test_decompose_fires_birth_hook_per_created_child(tmp_path, monkeypatch, capture_born):
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [{**_EPIC, "cwd": str(tmp_path)}]}) + "\n")
    _route_graph(g, tmp_path, monkeypatch)

    r = _invoke("backlog", "decompose", "ab-epic0001", "--groups", _GROUPS)
    assert r.exit_code == 0, r.output

    assert len(capture_born) == 2
    assert all(nid for nid, _ in capture_born)  # each carried a real child id
    rss = [rs for _, rs in capture_born]
    assert rss[0] is rss[1] is not None  # one shared RunState bounds the batch


def test_redecompose_update_does_not_refire_birth_hook(tmp_path, monkeypatch, capture_born):
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [{**_EPIC, "cwd": str(tmp_path)}]}) + "\n")
    _route_graph(g, tmp_path, monkeypatch)

    # First pass: 2 children created -> 2 births.
    assert _invoke("backlog", "decompose", "ab-epic0001", "--groups", _GROUPS).exit_code == 0
    assert len(capture_born) == 2
    capture_born.clear()

    # Identical re-run: every child is 'updated', none created -> no births.
    assert _invoke("backlog", "decompose", "ab-epic0001", "--groups", _GROUPS).exit_code == 0
    assert capture_born == []


# -- intake --

def test_intake_fires_birth_hook_once(tmp_path, monkeypatch, capture_born):
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    _route_graph(g, tmp_path, monkeypatch)

    plan = tmp_path / "plan.md"
    plan.write_text("---\ntitle: A Plan\n---\n# Body\n")

    r = _invoke("backlog", "intake", str(plan))
    assert r.exit_code == 0, r.output

    assert len(capture_born) == 1
    assert capture_born[0][0]  # the intaked node's real id reached the hook


# -- add (born-with-why v2: cmd_add wiring, x-a552) --

def test_add_fires_birth_hook_once(tmp_path, monkeypatch, capture_born):
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    _route_graph(g, tmp_path, monkeypatch)

    r = _invoke("backlog", "add", "A new feature", "--project", "fno", "--cwd", "/tmp/proj")
    assert r.exit_code == 0, r.output

    assert len(capture_born) == 1
    assert capture_born[0][0]  # the added node's real id reached the hook
