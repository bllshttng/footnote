"""`fno backlog lanes` - the parallel-lane status rollup (x-42d5 G4, US5).

Seeds live lane slots into a tmp claims root and a tmp graph, then asserts the
rollup joins them (lane -> node slug/status) and degrades cleanly when a lane's
node is unknown to the graph.
"""
from __future__ import annotations

import json
import subprocess

import pytest
from typer.testing import CliRunner

import fno.graph.cli as gcli
from fno.rust_binary import resolve_binary
from tests.fixtures.graph_seed import seed_graph

_runner = CliRunner()


def _seed_lane(lane_id: str, max_lanes: int, domain: str) -> None:
    """Hold a live slot through the native verb under the env-pinned root."""
    binary = resolve_binary()
    assert binary is not None, "dev binary required for lane seeding"
    subprocess.run(
        [str(binary), "claim", "lane-acquire", "--lane", lane_id,
         "--max-lanes", str(max_lanes), "--domain", domain],
        capture_output=True, text=True, check=True,
    )


@pytest.fixture
def claims_root(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    (tmp_path / "claims").mkdir()
    return tmp_path


@pytest.fixture
def graph(tmp_path, monkeypatch):
    path = tmp_path / "graph.json"
    seed_graph(path, [{
        "id": "x-aaaa",
        "slug": "alpha-work",
        "title": "Alpha work",
        "type": "feature",
        "priority": "p2",
        "status": "in_progress",
        "domain": "code",
    }])
    monkeypatch.setattr(gcli, "_graph_path", lambda: path)
    return path


@pytest.mark.dev_build
def test_lanes_rollup_joins_slots_with_graph(claims_root, graph):
    _seed_lane("x-aaaa", 3, "code")
    _seed_lane("x-bbbb", 3, "docs")

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


@pytest.mark.dev_build
def test_lanes_rollup_human_line(claims_root, graph):
    _seed_lane("x-aaaa", 2, "code")
    res = _runner.invoke(gcli.cli, ["lanes"])
    assert res.exit_code == 0, res.output
    assert "1/" in res.output.splitlines()[0]
    assert "x-aaaa" in res.output
    assert "domain=code" in res.output
