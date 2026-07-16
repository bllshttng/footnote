"""Unit tests for the `fno plan sync` whole-vault converger sweep (x-5d84).

Covers AC2-HP (sweep converges the vault), AC1-EDGE (no plan_path skipped),
AC2-EDGE (idempotent no-op), AC3-EDGE (--all bypasses the watermark), AC1-FR
(graph re-read failure degrades cleanly), AC2-FR (mtime short-circuit).
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.plan._stamp import read_plan_file

runner = CliRunner()

_PLAN = """\
---
node: {node}
status: ready
priority: {prio}
type: feature
---

# plan body
"""


@pytest.fixture
def env(tmp_path, monkeypatch):
    """A temp graph.json + a plans dir; graph_json() points at the temp graph so
    the sync command and its watermark both resolve under tmp_path."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.paths as paths
    monkeypatch.setattr(paths, "graph_json", lambda: g)
    return tmp_path, g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")
    # Bump graph mtime so the watermark gate sees "changed" between edits.
    os.utime(g, None)


def _doc(tmp_path: Path, name: str, node: str, prio: str) -> Path:
    p = tmp_path / name
    p.write_text(_PLAN.format(node=node, prio=prio), encoding="utf-8")
    return p


def test_sweep_converges_vault(env):
    """AC2-HP: three drifted docs all repaint; the receipt reports the count."""
    tmp_path, g = env
    docs = []
    entries = []
    for i in range(3):
        d = _doc(tmp_path, f"p{i}.md", f"x-000{i}", "p3")  # doc says p3
        docs.append(d)
        entries.append({"id": f"x-000{i}", "slug": f"s{i}", "plan_path": str(d),
                        "priority": "p1", "_status": "ready"})  # graph says p1
    _seed(g, entries)

    res = runner.invoke(app, ["plan", "sync"])
    assert res.exit_code == 0, res.output
    assert "3 docs repainted" in res.output
    for d in docs:
        _, fields, _ = read_plan_file(d)
        assert fields["priority"] == "p1"


def test_no_plan_path_skipped(env):
    """AC1-EDGE: a node without plan_path is not visited; no file created."""
    tmp_path, g = env
    _seed(g, [{"id": "x-0001", "slug": "s", "plan_path": None, "priority": "p0"}])

    res = runner.invoke(app, ["plan", "sync"])
    assert res.exit_code == 0, res.output
    assert "0 docs repainted" in res.output
    assert not (tmp_path / "p0.md").exists()


def test_idempotent_second_run(env):
    """AC2-EDGE: a converged vault rewrites zero files on the second sweep."""
    tmp_path, g = env
    d = _doc(tmp_path, "p.md", "x-0001", "p3")
    _seed(g, [{"id": "x-0001", "slug": "s", "plan_path": str(d),
               "priority": "p1", "_status": "ready"}])

    first = runner.invoke(app, ["plan", "sync", "--all"])  # --all: no gate
    assert "1 docs repainted" in first.output
    second = runner.invoke(app, ["plan", "sync", "--all"])
    assert "0 docs repainted" in second.output


def test_watermark_short_circuits_unchanged_graph(env):
    """AC2-FR: an unchanged graph (mtime <= watermark) is skipped, no doc read."""
    tmp_path, g = env
    d = _doc(tmp_path, "p.md", "x-0001", "p3")
    _seed(g, [{"id": "x-0001", "slug": "s", "plan_path": str(d),
               "priority": "p1", "_status": "ready"}])

    first = runner.invoke(app, ["plan", "sync"])
    assert "1 docs repainted" in first.output

    # Drift the doc AGAIN but do NOT touch the graph; the gate must skip it.
    d.write_text(_PLAN.format(node="x-0001", prio="p3"), encoding="utf-8")
    second = runner.invoke(app, ["plan", "sync"])
    assert "graph unchanged" in second.output
    _, fields, _ = read_plan_file(d)
    assert fields["priority"] == "p3"  # untouched: gate skipped the sweep


def test_all_bypasses_watermark(env):
    """AC3-EDGE: --all forces the walk even when the graph mtime is stale."""
    tmp_path, g = env
    d = _doc(tmp_path, "p.md", "x-0001", "p3")
    _seed(g, [{"id": "x-0001", "slug": "s", "plan_path": str(d),
               "priority": "p1", "_status": "ready"}])

    runner.invoke(app, ["plan", "sync"])  # sets watermark to current graph mtime
    d.write_text(_PLAN.format(node="x-0001", prio="p3"), encoding="utf-8")  # re-drift doc only

    res = runner.invoke(app, ["plan", "sync", "--all"])  # graph unchanged, but --all
    assert res.exit_code == 0, res.output
    assert "1 docs repainted" in res.output
    _, fields, _ = read_plan_file(d)
    assert fields["priority"] == "p1"


def test_graph_unreadable_degrades(env, monkeypatch):
    """AC1-FR: a transient graph read failure warns, repaints zero, exits 0."""
    tmp_path, g = env
    _seed(g, [{"id": "x-0001", "slug": "s", "plan_path": str(g), "priority": "p1"}])

    import fno.graph.store as gs

    def _boom(_path):
        raise OSError("transient")

    monkeypatch.setattr(gs, "read_graph", _boom)
    res = runner.invoke(app, ["plan", "sync"])
    assert res.exit_code == 0, res.output
    assert "unreadable" in res.output
    # Watermark NOT advanced -> next sweep re-converges.
    assert not (g.parent / ".plan-sync-watermark").exists()
