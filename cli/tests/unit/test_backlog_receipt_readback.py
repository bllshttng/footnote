"""Receipts must be backed by a store read-back."""

from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
from pathlib import Path

from typer.testing import CliRunner

from fno.cli import app


runner = CliRunner()


def _graph(tmp_path: Path, monkeypatch, entries: list[dict]) -> Path:
    graph = tmp_path / "graph.json"
    seed_graph(graph, json.dumps({"entries": entries}) + "\n")
    import fno.graph._constants as constants
    import fno.graph.store as store

    monkeypatch.setattr(constants, "GRAPH_JSON", graph)
    monkeypatch.setattr(constants, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(store, "GRAPH_JSON", graph)
    return graph


def test_idea_refuses_when_the_new_row_does_not_read_back(tmp_path, monkeypatch):
    _graph(tmp_path, monkeypatch, [])
    monkeypatch.setattr("fno.graph.store._readback_row", lambda path, node_id: (None, True))

    result = runner.invoke(
        app,
        ["backlog", "idea", "lost filing", "--difficulty", "low", "--separate"],
    )

    assert result.exit_code == 1, result.output
    assert "write did not land" in result.output


def test_update_receipt_names_the_resolved_id(tmp_path, monkeypatch):
    """The native verb resolves a prefix and the receipt names the resolved
    id (the read-back refusal and resolved-id contracts of the retired
    python leg ride the Rust goldens now)."""
    graph = _graph(
        tmp_path,
        monkeypatch,
        [{"id": "ab-00000001", "title": "old", "domain": "code", "project": "p"}],
    )
    import os as _os
    import subprocess as _sp

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    proc = _sp.run(
        [str(binary), "backlog", "update", "ab-0000", "--title", "new"],
        capture_output=True,
        text=True,
        timeout=60,
        env={
            "PATH": _os.environ["PATH"],
            "HOME": str(tmp_path),
            "FNO_STATE_DIR": str(tmp_path),
            "FNO_TRACKER_BACKEND": "graph",
        },
        cwd=str(tmp_path),
    )
    out = proc.stdout + proc.stderr
    assert proc.returncode == 0, out
    assert "Updated ab-00000001" in out
    assert "Updated ab-0000" not in out.splitlines()
