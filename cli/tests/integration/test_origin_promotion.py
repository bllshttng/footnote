"""Integration: request origin survives capture promotion (x-1005).

Drives the real typer app (capture add -> capture promote -> backlog read)
against hermetic graph/inbox roots, then asserts the node's birth carries the
capture's producing reference and an in-vocabulary origin. AC2-HP / AC2-EDGE.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

import fno.graph._constants as gc
import fno.graph.store as gs
from fno.cli import app

runner = CliRunner()


def _invoke(*args):
    return runner.invoke(app, list(args), catch_exceptions=False)


@pytest.fixture
def hermetic(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Route the graph AND the capture inbox into tmp roots."""
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": []}), encoding="utf-8")
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setattr("fno.backlog.capture._inbox_path", lambda: tmp_path / "inbox.md")
    monkeypatch.setattr("fno.backlog.capture._events_path", lambda: tmp_path / "events.jsonl")
    return g


def _entries(g: Path) -> list[dict]:
    return json.loads(g.read_text(encoding="utf-8"))["entries"]


def test_ac2_hp_promotion_preserves_capture_evidence(hermetic: Path):
    """A capture promoted in a later session keeps the item's substrate
    reference as birth evidence; the recorder is never the requester."""
    added = _invoke(
        "backlog", "capture", "add",
        "Ship the origin badges",
        "--source", "PR#1700",
        "--why", "operator asked to see request origin",
        "-p", "p2",
    )
    assert added.exit_code == 0, added.output
    fu_id = json.loads(added.output)["id"]

    promoted = _invoke(
        "backlog", "capture", "promote", fu_id,
        "--source-kind", "operator_request",
        "--difficulty", "medium",
    )
    assert promoted.exit_code == 0, promoted.output
    node_id = json.loads(promoted.output)["node_id"]

    from fno.graph.store import read_graph

    node = next(e for e in read_graph(hermetic) if e["id"] == node_id)
    assert node["source_kind"] == "operator_request"
    assert node["origin_evidence"] == f"{fu_id} source: PR#1700"
    from fno.graph._constants import REQUEST_ORIGINS

    assert node["request_origin"] in REQUEST_ORIGINS


def test_ac2_hp_promotion_without_source_line_still_names_the_fu_id(hermetic: Path):
    """The capture itself is the producing event: an item with no source
    sub-line still promotes with its fu-id as the evidence reference."""
    inbox = hermetic.parent / "inbox.md"
    inbox.parent.mkdir(parents=True, exist_ok=True)
    inbox.write_text(
        "- [ ] fu-7a3d9c - Bare capture (p2)\n  why: no source line recorded\n",
        encoding="utf-8",
    )

    promoted = _invoke(
        "backlog", "capture", "promote", "fu-7a3d9c",
        "--difficulty", "medium",
    )
    assert promoted.exit_code == 0, promoted.output
    node_id = json.loads(promoted.output)["node_id"]

    from fno.graph.store import read_graph

    node = next(e for e in read_graph(hermetic) if e["id"] == node_id)
    assert node["origin_evidence"] == "fu-7a3d9c"


def test_ac2_edge_repromotion_is_idempotent_and_birth_stable(hermetic: Path):
    _invoke(
        "backlog", "capture", "add",
        "Once only",
        "--source", "doc.md#section",
        "--why", "dedup guard",
    )
    fu_id = _first_fu(hermetic.parent / "inbox.md")
    first = _invoke(
        "backlog", "capture", "promote", fu_id,
        "--source-kind", "operator_request",
        "--difficulty", "medium",
    )
    assert first.exit_code == 0, first.output
    node_id = json.loads(first.output)["node_id"]

    second = _invoke(
        "backlog", "capture", "promote", fu_id,
        "--difficulty", "medium",
    )
    assert second.exit_code == 0, second.output
    receipt = json.loads(second.output)
    assert receipt["status"] == "already_promoted"
    assert receipt["node_id"] == node_id

    from fno.graph.store import read_graph

    node = next(e for e in read_graph(hermetic) if e["id"] == node_id)
    assert node["request_origin"] in ("operator_request", "unknown")
    assert node["origin_evidence"]


def _first_fu(inbox: Path) -> str:
    import re

    m = re.search(r"- \[ \] (fu-[0-9a-f]{6}) -", inbox.read_text(encoding="utf-8"))
    assert m, f"no open fu- item in {inbox}"
    return m.group(1)
