"""Unit tests for ``fno doctor graph backend`` (task 10.1). The soak
evidence read lives in Rust (``backlog::soak_gaps``, tested there); the
source-tree gates run from scripts/ci/check-graph-flip-gates.sh. Here the
flip verbs run against a real keeper on a temp graph with the gate seam
stubbed, so the operator's live graph and config are never touched."""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest
import typer

from fno import doctor_graph

FULL = {
    "type": "feature",
    "status": "idea",
    "priority": "p2",
    "domain": "code",
    "created_at": "2026-09-11T00:00:00+00:00",
}


def _row(node_id: str, **overrides):
    return {
        **FULL,
        "id": node_id,
        "slug": node_id,
        "title": node_id,
        "tags": [],
        **overrides,
    }


def _meta(graph: Path, key: str):
    try:
        with sqlite3.connect(graph.with_suffix(".db")) as connection:
            row = connection.execute(
                "SELECT value FROM graph_meta WHERE key = ?", (key,)
            ).fetchone()
    except sqlite3.OperationalError:
        # An untouched db carries no tables yet: the key reads unset.
        return None
    return row[0] if row else None


@pytest.fixture
def world(tmp_path, monkeypatch):
    """A temp machine: graph and state root, with the gate seam and the
    config writer stubbed. Skips where no keeper binary can spawn."""
    from fno.graph.store import _worker_binary

    if _worker_binary() is None:
        pytest.skip("no fno-agents-worker binary; build with `cargo build -p fno-agents`")
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps({"entries": [_row("x-1", title="one"), _row("x-2", title="two")]}),
        encoding="utf-8",
    )
    config_sets: list[tuple[str, str]] = []
    gaps: list[str] = []
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    monkeypatch.setattr(
        "fno.config.writer.set_config_value",
        lambda key, value, **k: config_sets.append((key, value)),
    )
    monkeypatch.setattr(
        doctor_graph, "_gate_gaps", lambda client, root: list(gaps)
    )
    return {
        "graph": graph,
        "tmp": tmp_path,
        "config_sets": config_sets,
        "gaps": gaps,
    }


def test_happy_flip_to_sqlite_stamps_backend_and_config(world):
    doctor_graph._flip("sqlite")
    assert _meta(world["graph"], "backend") == "sqlite"
    assert _meta(world["graph"], "backend_since_ms") is not None
    assert ("graph.read_source", "sqlite") in world["config_sets"]


def test_flip_refuses_naming_the_gap_and_changes_nothing(world):
    world["gaps"].append("first sample 2026-09-07T12:00:00Z is 2 day(s) old; the soak needs 7 days")
    with pytest.raises(typer.Exit):
        doctor_graph._flip("sqlite")
    assert _meta(world["graph"], "backend") is None
    assert world["config_sets"] == []


def test_flip_is_idempotent_and_keeps_the_since_stamp(world):
    doctor_graph._flip("sqlite")
    since_first = _meta(world["graph"], "backend_since_ms")
    doctor_graph._flip("sqlite")
    assert _meta(world["graph"], "backend_since_ms") == since_first


def test_rollback_exports_first_then_flips(world):
    from fno.graph.store import _client_for

    doctor_graph._flip("sqlite")
    client = _client_for(world["graph"])
    client.request(
        "op",
        {
            "name": "append_progress_note",
            "params": {
                "node_id": "x-1",
                "note": {"ts": "2026-09-14T12:00:00Z", "text": "flip probe"},
            },
        },
    )
    body = world["graph"].read_text(encoding="utf-8")
    assert "flip probe" not in body, "no background export: graph.json is frozen"
    doctor_graph._flip("json")
    body = world["graph"].read_text(encoding="utf-8")
    assert "flip probe" in body, "rollback exports current rows before the flip"
    assert _meta(world["graph"], "backend") == "json"
    assert ("graph.read_source", "json") in world["config_sets"]


def test_status_prints_the_status_line(world, capsys):
    doctor_graph._flip("sqlite")
    capsys.readouterr()
    doctor_graph.graph_backend(None, True)
    out = capsys.readouterr().out
    assert out.startswith("backend=sqlite since=")
    assert " days=0 keepers=" in out


def test_gate_gaps_unions_keeper_and_script_gaps(tmp_path, monkeypatch):
    """Keeper gap lines and the gate script's `flip-gate: FAIL:` lines both
    land in the refusal list; a crashed script is a gap of its own."""

    class FakeClient:
        def request(self, method, params):
            assert method == "backend_gate"
            return {"gaps": ["keeper gap"]}

    class FakeProc:
        returncode = 1
        stdout = "noise\nflip-gate: FAIL: reader census verbs failed\n"
        stderr = ""

    monkeypatch.setattr(
        doctor_graph.subprocess,
        "run",
        lambda *a, **k: FakeProc(),
    )
    gaps = doctor_graph._gate_gaps(FakeClient(), tmp_path)
    assert gaps == ["keeper gap", "reader census verbs failed"]
