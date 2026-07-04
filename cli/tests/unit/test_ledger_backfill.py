"""Tests for scripts/metrics/backfill-ledger-node-id.py resolution logic.

Epic x-f063 Wave 1 (ledger trust), change 5. Covers:
  AC1-UI    resolved/unrecoverable counts printed per pattern.
  AC1-EDGE  a title matching >1 graph node is stamped unrecoverable, not guessed.
  AC1-FR    the backfill is idempotent (a re-run changes nothing).
"""
from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path


def _load():
    repo_root = Path(__file__).resolve().parents[3]
    script_path = repo_root / "scripts" / "metrics" / "backfill-ledger-node-id.py"
    spec = importlib.util.spec_from_file_location("backfill_node_id", script_path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["backfill_node_id"] = mod
    spec.loader.exec_module(mod)
    return mod


NODES = {"ab-16daa753", "x-2c17", "x-bb53"}


def test_resolve_from_title():
    mod = _load()
    row = {"title": "Screen manifest fallback (ab-16daa753)", "branch": "main"}
    assert mod.resolve_row(row, NODES) == ("title", "ab-16daa753")


def test_resolve_from_branch_when_title_barren():
    mod = _load()
    row = {"title": "no id here", "branch": "feature/x-2c17-manifest"}
    assert mod.resolve_row(row, NODES) == ("branch", "x-2c17")


def test_ambiguous_title_is_unrecoverable_not_guessed():
    # AC1-EDGE: two distinct existing nodes in one field -> never guess.
    mod = _load()
    row = {"title": "merge of ab-16daa753 and x-2c17", "branch": "main"}
    assert mod.resolve_row(row, NODES) == ("unrecoverable", None)


def test_token_absent_from_graph_is_unrecoverable():
    # A node-id-shaped token that is not a real graph node does not count.
    mod = _load()
    row = {"title": "ab-deadbeef ghost", "branch": "worktree-rcfe-cluster"}
    assert mod.resolve_row(row, NODES) == ("unrecoverable", None)


def test_backfill_writes_and_is_idempotent(tmp_path):
    mod = _load()
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [
        {"title": "ab-16daa753 ship", "branch": "main", "pr_number": 1},
        {"title": "no id", "branch": "worktree-x", "pr_url": "http://x/pull/2"},
        {"title": "ab-16daa753 non-ship (no pr)", "branch": "main"},
    ]}))

    assert mod.backfill(ledger, NODES, apply=True) == 0
    rows = json.loads(ledger.read_text())["entries"]
    assert rows[0]["graph_node_id"] == "ab-16daa753"  # resolved
    assert rows[1]["node_id_unrecoverable"] is True     # ship, no id -> stamped
    assert "graph_node_id" not in rows[2]               # non-ship row untouched
    assert "node_id_unrecoverable" not in rows[2]

    # Idempotent: a second apply resolves zero rows and leaves the file unchanged.
    before = ledger.read_text()
    mod.backfill(ledger, NODES, apply=True)
    assert ledger.read_text() == before
