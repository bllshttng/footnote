"""Integration: request origin survives capture promotion (x-1005).

Drives the real typer app (capture add -> capture promote -> backlog read)
against hermetic graph/inbox roots, then asserts the node's birth carries the
capture's producing reference and an in-vocabulary origin. AC2-HP / AC2-EDGE.
"""

from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

import fno.graph._constants as gc
import fno.graph.store as gs
from fno.cli import app
from fno.graph.store import read_graph_strict

runner = CliRunner()


def _invoke(*args):
    return runner.invoke(app, list(args), catch_exceptions=False)


@pytest.fixture
def hermetic(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Route the graph AND the capture inbox into tmp roots."""
    g = tmp_path / "graph.json"
    seed_graph(g, json.dumps({"entries": []}))
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setattr("fno.backlog.capture._inbox_path", lambda: tmp_path / "inbox.md")
    monkeypatch.setattr("fno.backlog.capture._events_path", lambda: tmp_path / "events.jsonl")
    return g


@pytest.fixture
def operator_turn(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, native_backlog_door) -> None:
    transcript = tmp_path / "transcript.jsonl"
    transcript.write_text(json.dumps({
        "type": "user", "uuid": "turn-1", "timestamp": "2026-09-24T00:00:00Z",
        "message": {"role": "user", "content": "status on your nodes?"},
    }) + "\n", encoding="utf-8")
    monkeypatch.setenv("FNO_OPERATOR_SESSION_ID", "fixture-session")
    monkeypatch.setenv("FNO_OPERATOR_HARNESS", "claude")
    monkeypatch.setenv("FNO_OPERATOR_TRANSCRIPT", str(transcript))
    monkeypatch.setenv("FNO_OPERATOR_CAPTURE_DIR", str(tmp_path / "operator-capture"))


def _entries(g: Path) -> list[dict]:
    return read_graph_strict(g)


def test_ac2_hp_promotion_preserves_capture_evidence(hermetic: Path, operator_turn: None):
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

    node = next(e for e in read_graph_strict(hermetic) if e["id"] == node_id)
    assert node["source_kind"] == "operator_request"
    assert node["origin_evidence"] == f"{fu_id} source: PR#1700"
    assert node["request_origin"] == "operator_request"


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

    from fno.graph.store import read_graph_strict

    node = next(e for e in read_graph_strict(hermetic) if e["id"] == node_id)
    assert node["origin_evidence"] == "fu-7a3d9c"


def test_ac2_edge_repromotion_is_idempotent_and_birth_stable(hermetic: Path, operator_turn: None):
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

    from fno.graph.store import read_graph_strict

    node = next(e for e in read_graph_strict(hermetic) if e["id"] == node_id)
    assert node["request_origin"] == "operator_request"
    assert node["origin_evidence"]


def test_ac4_err_promotion_refuses_empty_queue_without_striking_item(
    hermetic: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, native_backlog_door
):
    inbox = tmp_path / "inbox.md"
    inbox.write_text("- [ ] fu-7a3d9c - Keep this capture (p2)\n", encoding="utf-8")
    absent = tmp_path / "empty-transcript.jsonl"
    absent.write_text("", encoding="utf-8")
    monkeypatch.setenv("FNO_OPERATOR_SESSION_ID", "fixture-session")
    monkeypatch.setenv("FNO_OPERATOR_HARNESS", "claude")
    monkeypatch.setenv("FNO_OPERATOR_TRANSCRIPT", str(absent))
    monkeypatch.setenv("FNO_OPERATOR_CAPTURE_DIR", str(tmp_path / "operator-capture"))
    monkeypatch.setattr("fno.backlog.capture._inbox_path", lambda: inbox)
    refused = _invoke(
        "backlog", "capture", "promote", "fu-7a3d9c",
        "--source-kind", "operator_request", "--difficulty", "low",
    )
    assert refused.exit_code == 1
    assert "queue is empty" in refused.output
    assert _entries(hermetic) == []
    assert "- [ ] fu-7a3d9c" in inbox.read_text(encoding="utf-8")


def _first_fu(inbox: Path) -> str:
    import re

    m = re.search(r"- \[ \] (fu-[0-9a-f]{6}) -", inbox.read_text(encoding="utf-8"))
    assert m, f"no open fu- item in {inbox}"
    return m.group(1)
