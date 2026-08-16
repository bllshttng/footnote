"""Carveout node stamp: proven session->node attribution at capture time (x-40be).

A deferred row blocks only the close of the node stamped on it, so the stamp
must be PROVEN ownership, never a guess. The claim lockfile is the authority
(harness-agnostic: a codex or gemini executor's node:<id> claim names the
owner too), with the manifest match (find_held_node) as the fallback. These
pin the capture shapes: claimed node, manifest-held node, foreign/stale
manifest, ambiguous claims, ambient shell - plus legacy-row parsing.
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


def _acquire_node_claim(claims_root, node_id, holder, monkeypatch):
    """Acquire a real node:<id> claim under a tmp global claims root."""
    from pathlib import Path

    from fno.claims.core import acquire_claim
    from fno.claims.io import claims_dir

    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(claims_root))
    return acquire_claim(
        f"node:{node_id}", holder, ttl_ms=60_000, root=claims_dir(Path(claims_root))
    )


def test_add_carveout_stamps_the_held_node(tmp_path, monkeypatch):
    _no_ambient_session(monkeypatch, tmp_path)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", _SID)
    _write_manifest(tmp_path)
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="left-out work", storage_root=tmp_path
    )
    assert cv.node == "x-40be"
    assert read_carveouts(tmp_path)[0]["node"] == "x-40be"


def test_a_codex_worker_stamps_via_its_claim_not_the_manifest(tmp_path, monkeypatch):
    """The claim path is harness-agnostic (codex P1): a CODEX_SESSION_ID whose
    node:<id> claim is live stamps the row with no claude env and no manifest
    at all."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    monkeypatch.setenv("CODEX_SESSION_ID", "codex-sess-9")
    _acquire_node_claim(tmp_path / "claims", "x-cx01", "target-session:codex-sess-9", monkeypatch)
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="x", storage_root=tmp_path
    )
    assert cv.node == "x-cx01"


def test_a_codex_worker_stamps_via_its_thread_id_holder(tmp_path, monkeypatch):
    """Production shape: init anchors the node claim on CODEX_THREAD_ID (the
    durable codex identity), not CODEX_SESSION_ID. The holder and the env the
    stamp reads must intersect or every codex-filed row lands unattributed."""
    _no_ambient_session(monkeypatch, tmp_path)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-t7")
    _acquire_node_claim(tmp_path / "claims", "x-cx03", "target-session:thread-t7", monkeypatch)
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="x", storage_root=tmp_path
    )
    assert cv.node == "x-cx03"


def test_a_claim_held_by_another_session_stamps_nothing(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_SESSION_ID", "codex-sess-MINE")
    _acquire_node_claim(tmp_path / "claims", "x-cx02", "target-session:codex-sess-OTHER", monkeypatch)
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="x", storage_root=tmp_path
    )
    assert cv.node is None


def test_two_matching_claims_are_ambiguous_and_stamp_nothing(tmp_path, monkeypatch):
    """One session id holding two node claims cannot name THE owner; guessing
    would stamp the wrong node's close gate. Fall through unattributed."""
    monkeypatch.setenv("CODEX_SESSION_ID", "codex-sess-2")
    _acquire_node_claim(tmp_path / "claims", "x-a1", "target-session:codex-sess-2", monkeypatch)
    _acquire_node_claim(tmp_path / "claims", "x-a2", "target-session:codex-sess-2", monkeypatch)
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="x", storage_root=tmp_path
    )
    assert cv.node is None


def test_a_stale_claim_falls_back_to_the_manifest(tmp_path, monkeypatch):
    """Claim dead mid-run but manifest still provably this session: the work
    still belongs to that node."""
    _no_ambient_session(monkeypatch, tmp_path)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", _SID)
    _write_manifest(tmp_path)
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="x", storage_root=tmp_path
    )
    assert cv.node == "x-40be"


def _no_ambient_session(monkeypatch, tmp_path):
    """Hermetic: no harness session envs, and the claims lookup pointed at an
    empty tmp root so the machine's real ~/.fno/claims is never read."""
    from fno.harness_identity import AMBIENT_IDENTITY_ENV

    for var in (*AMBIENT_IDENTITY_ENV, "TARGET_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims-empty"))


def test_mismatched_session_leaves_the_row_unattributed(tmp_path, monkeypatch):
    """A stale or foreign manifest must never stamp a node this process does
    not hold - find_held_node's never-guess rule, pinned at the capture seam."""
    _no_ambient_session(monkeypatch, tmp_path)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-OTHER")
    _write_manifest(tmp_path)
    cv, _ = add_carveout(
        tmp_path, kind="deferred", description="x", storage_root=tmp_path
    )
    assert cv.node is None


def test_ambient_shell_files_an_unattributed_row(tmp_path, monkeypatch):
    _no_ambient_session(monkeypatch, tmp_path)
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
