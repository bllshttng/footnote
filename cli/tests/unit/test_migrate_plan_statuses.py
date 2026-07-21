"""Wave 3: legacy plan-status migration script.

Covers the mapping table, the graph-truth override (graph wins over the map),
unmapped-status reporting, and the byte-preserving --apply write.
"""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import pytest

_SCRIPT = Path(__file__).resolve().parents[3] / "scripts" / "maintenance" / "migrate-plan-statuses.py"


def _load_module():
    spec = importlib.util.spec_from_file_location("migrate_plan_statuses", _SCRIPT)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


mps = _load_module()


def _plan(root: Path, name: str, status: str, node: str | None = None) -> Path:
    d = root / "plans"
    d.mkdir(parents=True, exist_ok=True)
    p = d / name
    fm = ["---", f"status: {status}", "created: 2026-07-08"]
    if node:
        fm.append(f"node: {node}")
    fm.append("---")
    p.write_text("\n".join(fm) + "\n# Doc\n\nbody\n")
    return p


def test_mapping_families():
    assert mps._classify("COMPLETE", None, {}) == "done"
    assert mps._classify("ready-for-implementation", None, {}) == "ready"
    assert mps._classify("inp-progres", None, {}) == "in_progress"
    assert mps._classify("abandoned", None, {}) == "superseded"
    # Already canonical -> no change.
    assert mps._classify("ready", None, {}) is None
    # Unmapped -> None (reported by main, left untouched).
    assert mps._classify("banana", None, {}) is None


def test_graph_truth_wins_over_mapping():
    truth = {"x-aaaa": "done"}
    # Doc says ready, but the graph node is done -> done.
    assert mps._classify("ready", "x-aaaa", truth) == "done"


def test_graph_truth_reads_completed_and_superseded(tmp_path):
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [
        {"id": "x-done", "completed_at": "2026-07-01T00:00:00Z"},
        {"id": "x-sup", "superseded_by": "x-other"},
        {"id": "x-open", "plan_path": "p.md"},
    ]}))
    truth = mps._graph_truth(g, None)
    assert truth == {"x-done": "done", "x-sup": "superseded"}


def test_apply_writes_and_preserves_other_keys(tmp_path):
    root = tmp_path / "vault"
    _plan(root, "a.md", "COMPLETE")
    _plan(root, "b.md", "ready")  # canonical, untouched
    _plan(root, "c.md", "banana")  # unmapped, untouched
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}')

    rc = mps.main(["--root", str(root), "--graph", str(g), "--apply"])
    assert rc == 0

    from fno.plan._stamp import read_plan_file

    _, fa, _ = read_plan_file(root / "plans" / "a.md")
    assert fa["status"] == "done"
    assert fa["created"] == "2026-07-08"  # sibling key preserved

    _, fc, _ = read_plan_file(root / "plans" / "c.md")
    assert fc["status"] == "banana"  # unmapped left as-is


def test_dry_run_writes_nothing(tmp_path):
    root = tmp_path / "vault"
    p = _plan(root, "a.md", "COMPLETE")
    before = p.read_text()
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}')

    rc = mps.main(["--root", str(root), "--graph", str(g)])
    assert rc == 0
    assert p.read_text() == before  # dry-run default: no write


def test_graph_truth_does_not_rewrite_a_retired_spelling_at_its_own_rung():
    """x-3ad5: the truth override runs BEFORE the already-canonical check, so a
    doc stamped `archived` whose node is superseded would be rewritten purely to
    change spelling - turning the rename into a migration pass over the vault.
    """
    assert mps._classify("archived", "x-1", {"x-1": "superseded"}) is None
    assert mps._classify("shipped", "x-1", {"x-1": "in_review"}) is None


def test_graph_truth_still_fixes_a_genuinely_stale_rung():
    """The override's real job survives the guard above: a `ready` doc whose
    node is closed is stale at the RUNG, not merely in spelling.
    """
    assert mps._classify("ready", "x-1", {"x-1": "done"}) == "done"
    assert mps._classify("ready", "x-1", {"x-1": "superseded"}) == "superseded"
