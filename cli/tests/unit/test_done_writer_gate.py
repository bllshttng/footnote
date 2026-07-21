"""Regression guards for the done=merged invariant (x-47a3).

A backlog node may only close on MERGED evidence. Historically the finalize
ledger append shelled an ungated ``update --completed`` leg and closed nodes
at ship time, 2h before their PR merged. These tests pin the writers shut.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# AC1-HP: a ledger append never closes a graph node
# ---------------------------------------------------------------------------


def _seed_graph(home: Path, *, plan_path: str) -> Path:
    """Write a graph fixture whose node is the ledger entry's join target."""
    graph = home / ".fno" / "graph.json"
    graph.parent.mkdir(parents=True, exist_ok=True)
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "x-fixture",
                        "plan_path": plan_path,
                        "pr_number": 505,
                        "_status": "in_review",
                    }
                ]
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    return graph


def test_register_entry_leaves_graph_byte_identical(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A finalize ledger append must not write to graph.json at all.

    Two assertions, because either alone is weak. Byte-identity catches an
    in-process writer but is only as red as the subprocess it fails to spawn;
    the shell-out assertion catches the actual incident shape (the deleted leg
    shelled ``roadmap-tasks.py update --completed``) deterministically, without
    depending on whether that subprocess would have succeeded here.

    The deleted writer fired only when the entry carried plan_path AND
    pr_number, so the entry below is exactly its trigger shape.
    """
    from fno.cost import _register

    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))

    plan_path = str(tmp_path / "plan.md")
    graph = _seed_graph(tmp_path, plan_path=plan_path)
    before = graph.read_bytes()

    ledger = tmp_path / "ledger.json"
    monkeypatch.setattr(_register._paths, "ledger_json", lambda: ledger)

    shelled: list[list[str]] = []
    real_run = _register.subprocess.run

    def _spy(cmd, *a, **kw):
        shelled.append([str(c) for c in cmd] if isinstance(cmd, (list, tuple)) else [str(cmd)])
        return real_run(cmd, *a, **kw)

    monkeypatch.setattr(_register.subprocess, "run", _spy)

    _register.register_entry(
        {
            "type": "execution",
            "plan_path": plan_path,
            "pr_number": 505,
            "pr_url": "https://github.com/o/r/pull/505",
            "graph_node_id": "x-e4bc",
            "session_id": "sess-1",
            "root_path": str(tmp_path),
        }
    )

    assert ledger.exists(), "the ledger append itself must still land"
    assert graph.read_bytes() == before, "ledger append must not mutate graph.json"

    graph_writes = [
        c for c in shelled if any("roadmap-tasks" in p or "--completed" in p for p in c)
    ]
    assert not graph_writes, f"ledger append shelled a graph close: {graph_writes}"


def test_register_module_has_no_graph_sync_leg() -> None:
    """The sync helpers are deleted, not merely unreferenced."""
    from fno.cost import _register

    for name in ("_sync_to_graph", "_match_graph_node", "_normalize_plan_path"):
        assert not hasattr(_register, name), f"{name} must stay deleted"
