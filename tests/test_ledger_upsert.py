#!/usr/bin/env python3
"""x-88df: ledger upsert primitive (US1) + collapse rule (US2).

Run: python3 tests/test_ledger_upsert.py   OR   pytest tests/test_ledger_upsert.py
"""
import importlib.util
import json
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
REGISTER_TASK_PATH = REPO_ROOT / "cli" / "src" / "fno" / "cost" / "_register.py"

_spec = importlib.util.spec_from_file_location("register_task_x88df", REGISTER_TASK_PATH)
register_task = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(register_task)


def _point_ledger_at(tmp_path: Path):
    """Redirect _paths.ledger_json() to a temp file and return its Path."""
    ledger = tmp_path / "ledger.json"
    register_task._paths.ledger_json = lambda: ledger  # type: ignore[assignment]
    return ledger


def _rows(ledger: Path) -> list:
    return json.loads(ledger.read_text())["entries"]


# --- US1: upsert_ledger_pr, three branches --------------------------------

def test_upsert_creates_minimal_backstop_row(tmp_path):
    ledger = _point_ledger_at(tmp_path)
    out = register_task.upsert_ledger_pr("x-aaaa", 101, "http://pr/101", "fno", "2026-07-18T00:00:00Z")
    assert out == "created"
    rows = _rows(ledger)
    assert len(rows) == 1
    r = rows[0]
    assert r["graph_node_id"] == "x-aaaa"
    assert r["pr_number"] == 101
    assert r["pr_url"] == "http://pr/101"
    assert r["project"] == "fno"
    assert r["completed"] == "2026-07-18T00:00:00Z"
    assert r["backstop"] is True
    assert r["termination_reason"] == "reconcile-backstop"


def test_upsert_stamps_null_pr_row_without_clobbering(tmp_path):
    ledger = _point_ledger_at(tmp_path)
    # A finalize row exists with pr_number null and full-fidelity fields set.
    ledger.write_text(json.dumps({"entries": [{
        "type": "execution", "status": "done", "graph_node_id": "x-bbbb",
        "pr_number": None, "cost_usd": 1.23, "phases_completed": ["do", "ship"],
        "completed": "2026-07-18T01:00:00Z", "fno_id": "sess-b",
    }]}))
    out = register_task.upsert_ledger_pr("x-bbbb", 202, "http://pr/202", "fno", "2026-07-18T09:00:00Z")
    assert out == "stamped"
    rows = _rows(ledger)
    assert len(rows) == 1  # stamped in place, no new row
    r = rows[0]
    assert r["pr_number"] == 202
    assert r["pr_url"] == "http://pr/202"
    # AC2-EDGE: full-fidelity fields and the real completion time preserved.
    assert r["cost_usd"] == 1.23
    assert r["phases_completed"] == ["do", "ship"]
    assert r["completed"] == "2026-07-18T01:00:00Z"


def test_upsert_noops_when_this_pr_already_present(tmp_path):
    ledger = _point_ledger_at(tmp_path)
    ledger.write_text(json.dumps({"entries": [{
        "type": "execution", "graph_node_id": "x-cccc", "pr_number": 303,
        "pr_url": "http://pr/303",
    }]}))
    out = register_task.upsert_ledger_pr("x-cccc", 303, "http://pr/303", "fno", "2026-07-18T00:00:00Z")
    assert out == "already-present"
    rows = _rows(ledger)
    assert len(rows) == 1
    assert rows[0]["pr_number"] == 303  # unchanged


def test_upsert_never_stamps_a_failed_attempt(tmp_path):
    # codex P2: a resumed node whose earlier attempt finalized Budget/NoProgress
    # (null pr) and whose real delivery lost its ledger write. The merged PR must
    # NOT be stamped onto the failed attempt - a fresh delivery backstop is made.
    ledger = _point_ledger_at(tmp_path)
    ledger.write_text(json.dumps({"entries": [{
        "type": "execution", "graph_node_id": "x-gggg", "pr_number": None,
        "termination_reason": "Budget", "cost_usd": 5.0,
    }]}))
    out = register_task.upsert_ledger_pr("x-gggg", 707, "http://pr/707", "fno", "2026-07-18T00:00:00Z")
    assert out == "created"
    rows = _rows(ledger)
    assert len(rows) == 2  # failed attempt preserved, delivery backstop added
    failed = next(r for r in rows if r.get("termination_reason") == "Budget")
    delivery = next(r for r in rows if r.get("backstop"))
    assert failed["pr_number"] is None  # NOT stamped
    assert delivery["pr_number"] == 707


# --- US2: collapse rule in append_to_tasks_json ---------------------------

def test_collapse_full_row_supersedes_backstop(tmp_path):
    ledger = tmp_path / "ledger.json"
    # A reconcile backstop row exists for the node.
    ledger.write_text(json.dumps({"entries": [{
        "type": "execution", "graph_node_id": "x-dddd", "pr_number": 404,
        "backstop": True, "termination_reason": "reconcile-backstop",
    }]}))
    # A full-fidelity finalize row for the same node lands.
    register_task.append_to_tasks_json(ledger, {
        "type": "execution", "status": "done", "graph_node_id": "x-dddd",
        "pr_number": 404, "cost_usd": 2.5, "phases_completed": ["do", "ship"],
        "fno_id": "sess-d",
    })
    rows = _rows(ledger)
    assert len(rows) == 1  # backstop dropped
    r = rows[0]
    assert r.get("backstop") is None  # the survivor is the full row
    assert r["cost_usd"] == 2.5
    assert r["fno_id"] == "sess-d"


def test_collapse_leaves_other_nodes_backstops_intact(tmp_path):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [{
        "type": "execution", "graph_node_id": "x-eeee", "pr_number": 505,
        "backstop": True,
    }]}))
    # Full row for a DIFFERENT node must not touch x-eeee's backstop.
    register_task.append_to_tasks_json(ledger, {
        "type": "execution", "graph_node_id": "x-ffff", "pr_number": 606,
        "fno_id": "sess-f",
    })
    rows = _rows(ledger)
    assert len(rows) == 2
    assert {r["graph_node_id"] for r in rows} == {"x-eeee", "x-ffff"}


if __name__ == "__main__":
    import sys
    import tempfile

    failed = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            with tempfile.TemporaryDirectory() as d:
                try:
                    fn(Path(d))
                    print(f"PASS {name}")
                except AssertionError as e:
                    failed += 1
                    print(f"FAIL {name}: {e}")
    sys.exit(1 if failed else 0)
