"""Integration tests for graph state hygiene safeguards (Phase 01).

Tests cover:
  - Hash sidecar written by locked_mutate_graph
  - load_graph() validates SHA256 hash
  - GraphCorruptionError raised on hash mismatch
  - First-run lazy sidecar creation
  - fno backlog rehash command (rehash and --revert)
  - Backup rotation (last 10 backups)
  - PreToolUse hook blocks/allows edits to graph.json
"""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _make_graph(path: Path, entries: list | None = None) -> None:
    """Write a minimal graph.json to path."""
    path.parent.mkdir(parents=True, exist_ok=True)
    data = {"entries": entries or []}
    path.write_text(json.dumps(data, indent=2) + "\n")


# ---------------------------------------------------------------------------
# Task 1.1 / 1.5: sidecar + backup
# ---------------------------------------------------------------------------

def test_locked_mutate_writes_sidecar(tmp_path):
    """After a locked_mutate_graph call, sidecar hash == SHA256 of graph.json."""
    from fno.graph.load import load_graph, GraphCorruptionError

    graph_path = tmp_path / "graph.json"
    sidecar_path = Path(str(graph_path) + ".sha256")

    # Use locked_mutate_graph to do a no-op add (bootstrap an entry)
    from fno.graph.store import locked_mutate_graph

    def _add_entry(entries):
        entries.append({"id": "ab-test01", "title": "Test 01", "status": "ready"})
        return entries

    locked_mutate_graph(graph_path, _add_entry)

    assert sidecar_path.exists(), "sidecar should be written after mutation"
    actual_hash = _sha256(graph_path)
    stored_hash = sidecar_path.read_text().strip()
    assert stored_hash == actual_hash, "sidecar must match actual graph.json hash"


def test_load_graph_validates_hash_match(tmp_path):
    """load_graph() succeeds when sidecar matches file content."""
    from fno.graph.load import load_graph
    from fno.graph.store import locked_mutate_graph

    graph_path = tmp_path / "graph.json"

    def _add_entry(entries):
        entries.append({"id": "ab-test02", "title": "Test 02", "status": "ready"})
        return entries

    locked_mutate_graph(graph_path, _add_entry)

    # Should not raise
    entries = load_graph(graph_path)
    assert len(entries) == 1
    assert entries[0]["id"] == "ab-test02"


def test_load_graph_raises_on_mismatch(tmp_path):
    """load_graph() raises GraphCorruptionError when graph.json is tampered."""
    from fno.graph.load import load_graph, GraphCorruptionError
    from fno.graph.store import locked_mutate_graph

    graph_path = tmp_path / "graph.json"

    def _add_entry(entries):
        entries.append({"id": "ab-test03", "title": "Test 03"})
        return entries

    locked_mutate_graph(graph_path, _add_entry)

    # Tamper graph.json without updating sidecar
    original = graph_path.read_bytes()
    graph_path.write_text('{"entries":[{"id":"ab-TAMPERED","title":"injected"}]}\n')

    with pytest.raises(GraphCorruptionError) as exc_info:
        load_graph(graph_path)

    err = exc_info.value
    assert err.actual != err.expected
    assert "rehash" in str(err).lower()


def test_first_run_writes_sidecar_lazily(tmp_path):
    """If graph.json exists but no sidecar, load_graph() writes sidecar and returns entries."""
    from fno.graph.load import load_graph

    graph_path = tmp_path / "graph.json"
    sidecar_path = Path(str(graph_path) + ".sha256")

    _make_graph(graph_path, [{"id": "ab-first", "title": "First run"}])
    assert not sidecar_path.exists(), "pre-condition: no sidecar"

    entries = load_graph(graph_path)
    assert len(entries) == 1
    assert entries[0]["id"] == "ab-first"
    assert sidecar_path.exists(), "sidecar should be lazily written"
    assert sidecar_path.read_text().strip() == _sha256(graph_path)


# ---------------------------------------------------------------------------
# Task 1.4: rehash command
# ---------------------------------------------------------------------------

def _patch_graph_path(monkeypatch, graph_path: Path) -> None:
    """Monkeypatch all graph-path constants to point at graph_path (tmp file).

    Mirrors the pattern used by test_graph_cli.py's tmp_graph fixture.
    """
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", graph_path.parent / "graph.lock")
    monkeypatch.setattr(gc, "GRAPH_MD", graph_path.parent / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", graph_path.parent / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", graph_path.parent / "graph.lock")


def test_rehash_rehashes_sidecar(tmp_path, monkeypatch):
    """After tamper, fno backlog rehash rehashes sidecar so load_graph() passes."""
    from fno.graph.load import load_graph
    from fno.graph.store import locked_mutate_graph

    graph_path = tmp_path / "graph.json"
    _patch_graph_path(monkeypatch, graph_path)

    def _add(entries):
        entries.append({"id": "ab-rec01", "title": "Reconcile test"})
        return entries

    locked_mutate_graph(graph_path, _add)

    # Tamper
    graph_path.write_text('{"entries":[{"id":"ab-tampered","title":"bad"}]}\n')

    # rehash via CLI (_graph_path() picks up gc.GRAPH_JSON monkeypatched above)
    result = runner.invoke(app, ["backlog", "rehash"])
    assert result.exit_code == 0, f"rehash failed: {result.output}"

    # load_graph should now pass
    entries = load_graph(graph_path)
    assert entries[0]["id"] == "ab-tampered"  # tampered content kept, hash updated


def test_rehash_revert_restores_backup(tmp_path, monkeypatch):
    """fno backlog rehash --revert restores graph.json from the last backup.

    Requires 2 mutations so that a backup of the good state exists before tamper.
    """
    from fno.graph.load import load_graph
    from fno.graph.store import locked_mutate_graph

    graph_path = tmp_path / "graph.json"
    _patch_graph_path(monkeypatch, graph_path)

    # First mutation: creates the file (no backup yet, file didn't exist)
    def _add_good(entries):
        entries.append({"id": "ab-good", "title": "Good entry"})
        return entries

    locked_mutate_graph(graph_path, _add_good)

    # Second mutation: triggers backup of the good state BEFORE writing new state
    def _add_second(entries):
        entries.append({"id": "ab-second", "title": "Second entry"})
        return entries

    locked_mutate_graph(graph_path, _add_second)

    # Now we have a backup of the state after the first mutation
    backups = sorted(tmp_path.glob("graph.json.bak.*"))
    assert len(backups) >= 1, "Expected at least one backup after second mutation"

    # Tamper
    graph_path.write_text('{"entries":[{"id":"ab-bad","title":"bad content"}]}\n')

    # Revert via CLI - should restore from the most-recent backup
    result = runner.invoke(app, ["backlog", "rehash", "--revert"])
    assert result.exit_code == 0, f"rehash --revert failed: {result.output}"

    # Should be back to good content (the backup, not the tampered content)
    entries = load_graph(graph_path)
    ids = [e["id"] for e in entries]
    assert "ab-bad" not in ids
    # The most-recent backup contains both ab-good and ab-second
    assert "ab-good" in ids


# ---------------------------------------------------------------------------
# Task 1.5: backup rotation
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
# Task 1.6/1.7: PreToolUse hook
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
    """Hook returns decision:approve for Edit targeting an unrelated file."""
    if not HOOK_SCRIPT.exists():
        pytest.skip("graph-write-protect.sh not yet created")

    payload = {
        "tool_name": "Edit",
        "tool_input": {"file_path": "/tmp/foo.txt"},
    }
    response = _invoke_hook(payload)
    assert response["decision"] == "approve"


def test_hook_allows_test_fixture_paths():
    """Hook returns decision:approve for graph.json paths under test/fixtures."""
    if not HOOK_SCRIPT.exists():
        pytest.skip("graph-write-protect.sh not yet created")

    payload = {
        "tool_name": "Edit",
        "tool_input": {"file_path": "/path/to/cli/tests/fixtures/graph.json"},
    }
    response = _invoke_hook(payload)
    assert response["decision"] == "approve"
