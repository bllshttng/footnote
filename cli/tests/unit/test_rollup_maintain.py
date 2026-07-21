"""The maintain rollup leg: propose-only backfill for standing orphans (US5)."""
from __future__ import annotations

from fno.graph.maintain import ROLLUP_PROPOSAL_CAP, detect_rollup_candidates


def node(nid, **kw):
    base = {"id": nid, "type": "feature", "title": nid, "status": "ready"}
    base.update(kw)
    return base


def epic(nid, title):
    return node(nid, type="epic", title=title)


def test_proposes_the_best_epic_for_an_orphan():
    entries = [
        epic("x-mux", "mux pane layout polish"),
        node("x-1", title="mux pane layout resize"),
    ]
    assert detect_rollup_candidates(entries) == [("x-1", "x-mux", _score(entries))]


def _score(entries):
    from fno.graph.relatedness import epic_candidates

    return epic_candidates(entries[1], entries, k=1)[0][1]


def test_linked_nodes_are_not_proposed():
    entries = [
        epic("x-mux", "mux pane layout polish"),
        node("x-1", title="mux pane layout resize", parent="x-mux"),
    ]
    assert detect_rollup_candidates(entries) == []


def test_orphan_with_no_candidate_is_absent():
    """This leg proposes links; the metric already counts the rest."""
    entries = [epic("x-mux", "mux pane layout polish"), node("x-1", title="quantum teapot")]
    assert detect_rollup_candidates(entries) == []


def test_exempt_nodes_are_not_proposed():
    entries = [
        epic("x-mux", "mux pane layout polish"),
        node("x-b", type="bug", title="mux pane layout resize"),
        node("x-d", title="mux pane layout resize", orphan_ok="spike"),
    ]
    assert detect_rollup_candidates(entries) == []


def test_closed_nodes_are_not_proposed():
    for status in ("done", "superseded", "deferred"):
        entries = [
            epic("x-mux", "mux pane layout polish"),
            node("x-1", title="mux pane layout resize", status=status),
        ]
        assert detect_rollup_candidates(entries) == [], status


def test_proposals_are_capped_and_best_first():
    entries = [epic("x-mux", "mux pane layout polish")]
    entries += [
        node(f"x-{i:03d}", title=f"mux pane layout polish variant {i}")
        for i in range(ROLLUP_PROPOSAL_CAP + 10)
    ]
    out = detect_rollup_candidates(entries)
    assert len(out) == ROLLUP_PROPOSAL_CAP
    assert [p[2] for p in out] == sorted((p[2] for p in out), reverse=True)


def test_respects_an_explicit_limit():
    entries = [epic("x-mux", "mux pane layout polish")]
    entries += [node(f"x-{i}", title="mux pane layout polish") for i in range(5)]
    assert len(detect_rollup_candidates(entries, limit=2)) == 2


def test_never_mutates_entries():
    entries = [
        epic("x-mux", "mux pane layout polish"),
        node("x-1", title="mux pane layout resize"),
    ]
    before = [dict(e) for e in entries]
    detect_rollup_candidates(entries)
    assert entries == before


def test_empty_graph_is_no_proposals():
    assert detect_rollup_candidates([]) == []


# -- CLI wiring (the leg must actually reach maintain's output) --


def test_maintain_cli_surfaces_rollup_candidates(tmp_path, monkeypatch):
    """Guards the propose-only leg's plumbing into `fno backlog maintain`."""
    import json

    from typer.testing import CliRunner

    import fno.graph._constants as gc
    import fno.graph.store as gs
    from fno.cli import app

    entries = [
        {**epic("x-mux0001", "mux pane layout polish"), "project": "fno",
         "cwd": "/tmp/proj", "priority": "p1", "blocked_by": [],
         "created_at": "2026-01-01T00:00:00+00:00"},
        {**node("x-orph0001", title="mux pane layout resize"), "project": "fno",
         "cwd": "/tmp/proj", "priority": "p2", "blocked_by": [],
         "created_at": "2026-01-01T00:00:00+00:00"},
    ]
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    for mod, attr, val in (
        (gc, "GRAPH_JSON", g), (gc, "GRAPH_MD", tmp_path / "g.md"),
        (gc, "GRAPH_HTML", tmp_path / "g.html"),
        (gs, "GRAPH_JSON", g),
    ):
        monkeypatch.setattr(mod, attr, val)

    res = CliRunner().invoke(app, ["backlog", "maintain"], catch_exceptions=False)

    assert res.exit_code == 0
    assert "rollup-candidates 1" in res.stdout
    assert "rollup candidate x-orph0001 -> x-mux0001" in res.stdout
    assert "--parent x-mux0001" in res.stdout
    # Propose-only: the graph is untouched.
    assert json.loads(g.read_text())["entries"][1].get("parent") is None
