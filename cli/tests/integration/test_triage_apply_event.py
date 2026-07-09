"""triage_applied telemetry from `fno backlog triage apply` (x-64cb US2, AC2-HP).

A proposal with one valid priority change and one entry referencing an unknown
id must (a) apply the valid change, (b) drop the invalid one, and (c) emit a
triage_applied event recording the applied counts, the from/to move list, and
dropped=1 - making _validate_proposal's previously-silent drops a first-class
number.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture()
def tmp_graph(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    for mod in (gc, gs):
        monkeypatch.setattr(mod, "GRAPH_JSON", g, raising=False)
        monkeypatch.setattr(mod, "GRAPH_LOCK_FILE", tmp_path / "graph.lock", raising=False)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md", raising=False)
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json", raising=False)
    return g


@pytest.fixture()
def events_file(tmp_path, monkeypatch):
    """Route triage_applied emits to a tmp events.jsonl so the test never
    writes to the real repo-root log."""
    ev = tmp_path / "events.jsonl"
    import fno.graph.triage as triage

    monkeypatch.setattr(triage, "_events_path", lambda: ev)
    return ev


def test_apply_emits_triage_applied_with_drop_count(tmp_graph, events_file, tmp_path):
    tmp_graph.write_text(
        json.dumps(
            {"entries": [{"id": "ab-1", "title": "N", "priority": "p2", "_status": "ready"}]}
        )
    )
    proposal = tmp_path / "proposal.json"
    proposal.write_text(
        json.dumps(
            {
                "priority_changes": [
                    {"id": "ab-1", "to": "p1", "reason": "align"},
                    {"id": "ab-UNKNOWN", "to": "p0", "reason": "bogus"},
                ]
            }
        )
    )

    r = runner.invoke(app, ["backlog", "triage", "apply", str(proposal)])
    # Partial apply (one entry dropped) exits 3 by design.
    assert r.exit_code == 3, r.output

    lines = [ln for ln in events_file.read_text().splitlines() if ln.strip()]
    events = [json.loads(ln) for ln in lines]
    applied_events = [e for e in events if e["type"] == "triage_applied"]
    assert len(applied_events) == 1
    data = applied_events[0]["data"]
    assert data["applied"]["priority_changes"] == 1
    assert data["dropped"] == 1
    assert data["proposed"] == 2
    assert data["priority_moves"] == [{"id": "ab-1", "from": "p2", "to": "p1"}]
    assert applied_events[0]["source"] == "backlog"


def test_apply_emit_failure_never_breaks_apply(tmp_graph, tmp_path, monkeypatch):
    # A broken events sink must not change apply semantics (best-effort).
    import fno.graph.triage as triage

    monkeypatch.setattr(triage, "_events_path", lambda: Path("/nonexistent-dir-xyz/does/not/exist/e.jsonl"))
    monkeypatch.setattr(triage, "_emit_triage_applied", triage._emit_triage_applied)
    tmp_graph.write_text(
        json.dumps({"entries": [{"id": "ab-1", "title": "N", "priority": "p2", "_status": "ready"}]})
    )
    proposal = tmp_path / "proposal.json"
    proposal.write_text(json.dumps({"priority_changes": [{"id": "ab-1", "to": "p1", "reason": "x"}]}))

    r = runner.invoke(app, ["backlog", "triage", "apply", str(proposal)])
    assert r.exit_code == 0, r.output  # clean apply, emit failure swallowed
    graph = json.loads(tmp_graph.read_text())
    assert graph["entries"][0]["priority"] == "p1"
