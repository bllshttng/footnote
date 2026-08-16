"""Carveout node stamp: proven session->node attribution at capture time (x-40be).

A deferred row blocks only the close of the node stamped on it, so the stamp
must be PROVEN ownership (find_held_node: the manifest's claude_session_id
matches the live process), never a guess. These pin the three capture shapes:
held node, foreign/stale manifest, ambient shell - plus legacy-row parsing.
"""
from __future__ import annotations

import json

from fno.carveout.core import add_carveout, read_carveouts

_SID = "sess-1234"


def _write_manifest(root, claude_session_id=_SID, graph_node_id="x-40be"):
    state = root / ".fno"
    state.mkdir(parents=True, exist_ok=True)
    lines = ["---"]
    if claude_session_id is not None:
        lines.append(f"claude_session_id: {claude_session_id}")
    if graph_node_id is not None:
        lines.append(f"graph_node_id: {graph_node_id}")
    lines.append("---")
    (state / "target-state.md").write_text("\n".join(lines) + "\n", encoding="utf-8")


def test_add_carveout_stamps_the_held_node(tmp_path, monkeypatch):
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", _SID)
    _write_manifest(tmp_path)
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="left-out work", storage_root=tmp_path
    )
    assert cv.node == "x-40be"
    assert read_carveouts(tmp_path)[0]["node"] == "x-40be"


def test_mismatched_session_leaves_the_row_unattributed(tmp_path, monkeypatch):
    """A stale or foreign manifest must never stamp a node this process does
    not hold - find_held_node's never-guess rule, pinned at the capture seam."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-OTHER")
    _write_manifest(tmp_path)
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="x", storage_root=tmp_path
    )
    assert cv.node is None


def test_ambient_shell_files_an_unattributed_row(tmp_path, monkeypatch):
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="x", storage_root=tmp_path
    )
    assert cv.node is None


def test_a_legacy_record_without_node_parses_as_none(tmp_path):
    """Existing JSONL rows predate the field; a missing key reads None so the
    close gate and the harvest never crash on them."""
    from fno.paths import project_log

    ledger = project_log("carveouts.jsonl", project_root=tmp_path)
    ledger.parent.mkdir(parents=True, exist_ok=True)
    legacy = {
        "id": "cv-old", "ts": "t", "session_id": None, "kind": "deferred",
        "priority": None, "need": None, "description": "old", "truncated": False,
        "scope": None, "severity": None,
    }
    ledger.write_text(json.dumps(legacy) + "\n", encoding="utf-8")
    rec = read_carveouts(tmp_path)[0]
    assert rec.get("node") is None
