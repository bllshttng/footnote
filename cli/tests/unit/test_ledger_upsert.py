#!/usr/bin/env python3
"""x-88df: ledger upsert primitive (US1) + collapse rule (US2).

Run: python3 tests/test_ledger_upsert.py   OR   pytest tests/test_ledger_upsert.py
"""
import importlib.util
import json
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
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


# --- project key for the backstop row -------------------------------------

def test_ledger_project_prefers_the_remote_slug(tmp_path, monkeypatch):
    import fno.graph._intake as intake
    import fno.paths as paths_mod

    monkeypatch.setattr(intake, "repo_root", lambda: "/repo")
    monkeypatch.setattr(
        paths_mod, "_slug_from_git_remote",
        lambda root: "footnote" if str(root) == "/repo" else None,
    )
    assert register_task.ledger_project_for({"project": "fno"}) == "footnote"


def test_ledger_project_falls_back_to_the_node_field(tmp_path, monkeypatch):
    import fno.graph._intake as intake
    import fno.paths as paths_mod

    monkeypatch.setattr(intake, "repo_root", lambda: "/repo")
    monkeypatch.setattr(paths_mod, "_slug_from_git_remote", lambda root: None)
    assert register_task.ledger_project_for({"project": "fno"}) == "fno"


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
