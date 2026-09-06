"""Integration tests for graph state hygiene safeguards (Phase 01).

Tests cover:
  - load_graph() reads entries through the keeper's gated read
  - A stale .sha256 sidecar left on disk is ignored, never read
  - Backup rotation (last 10 backups)
  - PreToolUse hook blocks/allows edits to graph.json
"""
from __future__ import annotations

import json
import os
import subprocess
import time
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_graph(path: Path, entries: list | None = None) -> None:
    """Write a minimal graph.json to path."""
    path.parent.mkdir(parents=True, exist_ok=True)
    data = {"entries": entries or []}
    path.write_text(json.dumps(data, indent=2) + "\n")


# ---------------------------------------------------------------------------
# Reads
# ---------------------------------------------------------------------------

def test_load_graph_reads_entries_after_a_mutation(tmp_path):
    """load_graph() returns the entries a locked_mutate_graph published."""
    from fno.graph.load import load_graph
    from fno.graph.store import locked_mutate_graph

    graph_path = tmp_path / "graph.json"

    def _add_entry(entries):
        entries.append({"id": "ab-test02", "title": "Test 02", "status": "ready"})
        return entries

    locked_mutate_graph(graph_path, _add_entry)

    entries = load_graph(graph_path)
    assert len(entries) == 1
    assert entries[0]["id"] == "ab-test02"


def test_stale_sidecar_on_disk_is_ignored(tmp_path):
    """A leftover graph.json.sha256 neither blocks a read nor gets rewritten.

    The sidecar gate is retired; whatever an older install left behind is an
    inert file. The mtime assert is the positive marker: nothing still writes
    the sidecar.
    """
    from fno.graph.load import load_graph
    from fno.graph.store import locked_mutate_graph

    graph_path = tmp_path / "graph.json"
    sidecar_path = Path(str(graph_path) + ".sha256")

    def _add_entry(entries):
        entries.append({"id": "ab-stale01", "title": "Stale sidecar"})
        return entries

    locked_mutate_graph(graph_path, _add_entry)
    sidecar_path.write_text("0" * 64 + "\n")
    before = sidecar_path.stat().st_mtime_ns

    entries = load_graph(graph_path)
    assert [e["id"] for e in entries] == ["ab-stale01"]

    time.sleep(0.01)
    assert sidecar_path.exists()
    assert sidecar_path.stat().st_mtime_ns == before
    assert sidecar_path.read_text().strip() == "0" * 64


# ---------------------------------------------------------------------------
# Backup rotation
# ---------------------------------------------------------------------------

def test_locked_mutate_keeps_last_10_backups(tmp_path):
    """After 15 sequential mutations, only 10 backups remain on disk."""
    from fno.graph.store import locked_mutate_graph

    graph_path = tmp_path / "graph.json"

    for i in range(15):
        idx = i  # capture for closure

        def _add(entries, _i=idx):
            entries.append({"id": f"ab-rot{_i:02d}", "title": f"Rotation {_i}"})
            return entries

        locked_mutate_graph(graph_path, _add)
        # Small sleep to ensure distinct timestamps in backup names
        time.sleep(0.01)

    backups = sorted(tmp_path.glob("graph.json.bak.*"))
    assert len(backups) == 10, f"Expected 10 backups, got {len(backups)}: {backups}"


# ---------------------------------------------------------------------------
# PreToolUse hook
# ---------------------------------------------------------------------------

HOOK_SCRIPT = Path(__file__).parent.parent.parent.parent / "hooks" / "graph-write-protect.sh"


def _invoke_hook(payload: dict) -> dict:
    """Run the hook script with the given payload, return parsed JSON output."""
    result = subprocess.run(
        [str(HOOK_SCRIPT)],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def test_hook_blocks_edit_to_graph():
    """Hook returns decision:block for Edit targeting ~/.fno/graph.json."""
    if not HOOK_SCRIPT.exists():
        pytest.skip("graph-write-protect.sh not yet created")

    payload = {
        "tool_name": "Edit",
        "tool_input": {"file_path": str(Path.home() / ".fno" / "graph.json")},
    }
    response = _invoke_hook(payload)
    assert response["decision"] == "block"
    assert "fno backlog" in response.get("reason", "")


def test_hook_allows_edit_to_unrelated_file():
    """Hook returns an empty allow response for an unrelated file."""
    if not HOOK_SCRIPT.exists():
        pytest.skip("graph-write-protect.sh not yet created")

    payload = {
        "tool_name": "Edit",
        "tool_input": {"file_path": "/tmp/foo.txt"},
    }
    response = _invoke_hook(payload)
    assert response == {}


def test_hook_allows_test_fixture_paths():
    """Hook returns an empty allow response for test graph fixtures."""
    if not HOOK_SCRIPT.exists():
        pytest.skip("graph-write-protect.sh not yet created")

    payload = {
        "tool_name": "Edit",
        "tool_input": {"file_path": "/path/to/cli/tests/fixtures/graph.json"},
    }
    response = _invoke_hook(payload)
    assert response == {}
