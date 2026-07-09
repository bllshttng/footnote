"""Wave 2.2: size capture (doc->graph) + cost capture (ledger->node at done).

- `normalize_size` coerces plan-frontmatter size to canonical S|M|L.
- `backlog intake` copies size from the plan frontmatter onto the new node.
- `backlog done` stamps cost_usd/cost_sessions from the ledger (reusing the
  same rollup `fno done` uses); a missing ledger row leaves cost null and never
  blocks the close.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.graph._intake import normalize_size

runner = CliRunner()


def _route_graph(tmp_path, monkeypatch) -> tuple[Path, Path]:
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return g, ledger


def _entries(g: Path) -> list[dict]:
    return json.loads(g.read_text())["entries"]


# -- normalize_size --------------------------------------------------------


@pytest.mark.parametrize(
    "value,expected",
    [
        ("S", "S"), ("m", "M"), ("l", "L"), (" M ", "M"),
        ("XL", None), ("", None), (None, None), ("medium", None),
    ],
)
def test_normalize_size(value, expected):
    assert normalize_size(value) == expected


# -- intake copies size doc->graph -----------------------------------------


def test_intake_copies_size_from_frontmatter(tmp_path, monkeypatch):
    g, _ = _route_graph(tmp_path, monkeypatch)
    plan = tmp_path / "plan.md"
    plan.write_text(
        "---\ncreated: 2026-05-05T04:35\nsize: l\n---\n# Sized plan\n\nBody.\n"
    )

    result = runner.invoke(app, ["backlog", "intake", str(plan)])
    assert result.exit_code == 0, result.output

    node = _entries(g)[0]
    assert node["size"] == "L"  # lowercase frontmatter -> canonical uppercase


def test_intake_no_size_frontmatter_leaves_null(tmp_path, monkeypatch):
    g, _ = _route_graph(tmp_path, monkeypatch)
    plan = tmp_path / "plan.md"
    plan.write_text("---\ncreated: 2026-05-05T04:35\n---\n# Unsized\n\nBody.\n")

    result = runner.invoke(app, ["backlog", "intake", str(plan)])
    assert result.exit_code == 0, result.output
    assert _entries(g)[0]["size"] is None


# -- backlog done stamps cost ledger->node ---------------------------------


def _seed_node(g: Path, plan_path: str) -> None:
    g.write_text(json.dumps({"entries": [{
        "id": "ab-cost0001",
        "title": "Costed node",
        "plan_path": plan_path,
        "cost_usd": None,
        "cost_sessions": [],
    }]}) + "\n")


def test_backlog_done_stamps_cost_from_ledger(tmp_path, monkeypatch):
    g, ledger = _route_graph(tmp_path, monkeypatch)
    _seed_node(g, "internal/plans/costed.md")
    ledger.write_text(json.dumps({"entries": [{
        "plan_path": "internal/plans/costed.md",
        "cost_usd": 1.20,
        "sessions": ["sess-a", "sess-b"],
        "completed": "2026-07-08T10:00:00Z",
    }]}) + "\n")

    # --force --skip-stamp bypasses the gh cross-check and plan stamp; the node
    # has no PR refs so no gh call is made.
    result = runner.invoke(
        app, ["backlog", "done", "ab-cost0001", "--force", "--reason", "test", "--skip-stamp"]
    )
    assert result.exit_code == 0, result.output

    node = _entries(g)[0]
    assert node["completed_at"]
    assert node["cost_usd"] == pytest.approx(1.20)
    assert len(node["cost_sessions"]) == 2


def test_backlog_done_does_not_overwrite_existing_cost(tmp_path, monkeypatch):
    """Fill-only: a node that already carries cost (e.g. from `fno done`) keeps
    it; backlog done never clobbers a richer prior stamp (codex P2)."""
    g, ledger = _route_graph(tmp_path, monkeypatch)
    g.write_text(json.dumps({"entries": [{
        "id": "ab-cost0001",
        "title": "Pre-costed",
        "plan_path": "internal/plans/costed.md",
        "cost_usd": 9.99,
        "cost_sessions": [{"session_id": "pre", "cost_usd": 9.99}],
    }]}) + "\n")
    ledger.write_text(json.dumps({"entries": [{
        "plan_path": "internal/plans/costed.md", "cost_usd": 1.20,
        "sessions": ["sess-a"], "completed": "2026-07-08T10:00:00Z",
    }]}) + "\n")

    result = runner.invoke(
        app, ["backlog", "done", "ab-cost0001", "--force", "--reason", "test", "--skip-stamp"]
    )
    assert result.exit_code == 0, result.output
    node = _entries(g)[0]
    assert node["cost_usd"] == pytest.approx(9.99)  # prior stamp preserved
    assert node["cost_sessions"] == [{"session_id": "pre", "cost_usd": 9.99}]


def test_backlog_done_no_ledger_row_leaves_cost_null(tmp_path, monkeypatch):
    g, _ = _route_graph(tmp_path, monkeypatch)
    _seed_node(g, "internal/plans/uncosted.md")  # ledger stays empty

    result = runner.invoke(
        app, ["backlog", "done", "ab-cost0001", "--force", "--reason", "test", "--skip-stamp"]
    )
    assert result.exit_code == 0, result.output

    node = _entries(g)[0]
    assert node["completed_at"]  # close still succeeds
    assert node["cost_usd"] is None
    assert node["cost_sessions"] == []
