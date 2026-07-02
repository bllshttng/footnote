"""`fno backlog lanes` - the parallel-lane status rollup (x-42d5 G4, US5).

Seeds live lane slots into a tmp claims root and a tmp graph, then asserts the
rollup joins them (lane -> node slug/status) and degrades cleanly when a lane's
node is unknown to the graph.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

import fno.graph.cli as gcli
from fno.claims.lanes import acquire_lane_slot

_runner = CliRunner()


@pytest.fixture
def claims_root(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    (tmp_path / "claims").mkdir()
    return tmp_path


@pytest.fixture
def graph(tmp_path, monkeypatch):
    path = tmp_path / "graph.json"
    path.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "x-aaaa",
                        "slug": "alpha-work",
                        "title": "Alpha work",
                        "status": "in-progress",
                        "domain": "code",
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    monkeypatch.setattr(gcli, "_graph_path", lambda: path)
    return path


def test_lanes_rollup_joins_slots_with_graph(claims_root, graph):
    acquire_lane_slot(3, "x-aaaa", extra_metadata={"domain": "code"})
    acquire_lane_slot(3, "x-bbbb", extra_metadata={"domain": "docs"})

    res = _runner.invoke(gcli.cli, ["lanes", "--json"])
    assert res.exit_code == 0, res.output
    out = json.loads(res.output)
    assert out["active"] == 2
    lanes = {ln["lane_id"]: ln for ln in out["lanes"]}
    assert lanes["x-aaaa"]["slug"] == "alpha-work"
    assert lanes["x-aaaa"]["domain"] == "code"
    # a lane whose node is not in the graph still renders (claims-only row)
    assert lanes["x-bbbb"]["slug"] is None
    assert lanes["x-bbbb"]["domain"] == "docs"


def test_lanes_rollup_empty(claims_root, graph):
    res = _runner.invoke(gcli.cli, ["lanes", "--json"])
    assert res.exit_code == 0, res.output
    out = json.loads(res.output)
    assert out["active"] == 0
    assert out["lanes"] == []


def test_lanes_rollup_human_line(claims_root, graph):
    acquire_lane_slot(2, "x-aaaa", extra_metadata={"domain": "code"})
    res = _runner.invoke(gcli.cli, ["lanes"])
    assert res.exit_code == 0, res.output
    assert "1/" in res.output.splitlines()[0]
    assert "x-aaaa" in res.output
    assert "domain=code" in res.output
