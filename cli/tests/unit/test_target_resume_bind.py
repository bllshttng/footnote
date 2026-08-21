"""Tests for ``fno.target.resume_bind`` + ``fno.target.manifest`` (x-2ccd wave 1).

The resume-bind primitive's identity contract: noop without a manifest, refuse
on identity mismatch / terminal node / foreign owner, rebind a same-session
local dead owner. The manifest reader/identity helpers are exercised directly.
"""
from __future__ import annotations

import os
import socket

import pytest

from fno.claims import claim_status
from fno.claims.hostid import machine_id
from fno.claims.io import claim_path, serialize_claim
from fno.claims.staleness import now_ms
from fno.claims.types import Claim
from fno.target import resume_bind as rb
from fno.target.manifest import manifest_identity

SID = "507dddb2-4559-4faa-8e34-ab94d627da8e"
HOLDER = f"target-session:{SID}"
KEY = "node:x-test-resume"
_DEAD_PID = 9_999_999


def _manifest(*, harness="claude", harness_session_id=SID, node="x-test-resume"):
    return {
        "harness": harness,
        "harness_session_id": harness_session_id,
        "fno_id": "20260803T072139Z-cl84140-079bf7",
        "graph_node_id": node,
        "target_claim_key": f"node:{node}",
        "target_claim_holder": f"target-session:{harness_session_id}",
    }


def _seed_stale_local(tmp_path, holder=HOLDER):
    """A local same-holder claim with a dead prior pid (the resume scenario)."""
    prior = Claim(
        schema_version=1,
        key=KEY,
        holder=holder,
        acquired_at=now_ms() - 200_000,
        expires_at=now_ms() - 100_000,
        pid=_DEAD_PID,
        host=socket.gethostname(),
        machine_id=machine_id() or None,
    )
    path = claim_path(KEY, root=tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_claim(prior), encoding="utf-8")
    return prior


def test_noop_when_no_manifest(tmp_path, monkeypatch):
    monkeypatch.setattr(rb, "read_target_manifest", lambda root: None)
    result = rb.resume_bind(tmp_path, harness="claude", harness_session_id=SID)
    assert result["result"] == "noop"


def test_refused_when_manifest_identity_incomplete(tmp_path, monkeypatch):
    monkeypatch.setattr(rb, "read_target_manifest", lambda root: {"harness": "claude"})
    result = rb.resume_bind(tmp_path, harness="claude", harness_session_id=SID)
    assert result["result"] == "refused"
    assert "identity" in result["reason"]


def test_refused_on_harness_mismatch(tmp_path, monkeypatch):
    monkeypatch.setattr(rb, "read_target_manifest", lambda root: _manifest(harness="codex"))
    result = rb.resume_bind(tmp_path, harness="claude", harness_session_id=SID)
    assert result["result"] == "refused"
    assert result["field"] == "harness"


def test_refused_on_session_id_mismatch(tmp_path, monkeypatch):
    """AC3: a different durable session is a foreign owner and fails closed."""
    monkeypatch.setattr(rb, "read_target_manifest", lambda root: _manifest())
    _seed_stale_local(tmp_path)
    result = rb.resume_bind(
        tmp_path, harness="claude", harness_session_id="foreign-session-id",
        claims_root=tmp_path,
    )
    assert result["result"] == "refused"
    assert result["field"] == "harness_session_id"


def test_rebind_same_session_local_dead_owner(tmp_path, monkeypatch):
    """AC1: the same durable session rebinds a dead local owner atomically."""
    monkeypatch.setattr(rb, "read_target_manifest", lambda root: _manifest())
    _seed_stale_local(tmp_path)
    new_pid = 4242
    result = rb.resume_bind(
        tmp_path, harness="claude", harness_session_id=SID, new_pid=new_pid,
        claims_root=tmp_path,
    )
    assert result["result"] == "rebound"
    assert result["pid"] == new_pid
    assert result["harness_session_id"] == SID
    rebound = claim_status(KEY, root=tmp_path)
    assert rebound["pid"] == new_pid
    # A fresh-lease rebind; live only when new_pid is a real live process.
    assert rebound["state"] in {"live", "suspect"}


def test_refused_on_offhost_owner(tmp_path, monkeypatch):
    """AC3: an off-host dead owner refuses (death unproven)."""
    monkeypatch.setattr(rb, "read_target_manifest", lambda root: _manifest())
    prior = Claim(
        schema_version=1,
        key=KEY,
        holder=HOLDER,
        acquired_at=now_ms(),
        expires_at=now_ms() + 60_000,
        pid=_DEAD_PID,
        host=socket.gethostname(),
        machine_id="00000000-0000-0000-0000-000000000000",
    )
    path = claim_path(KEY, root=tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_claim(prior), encoding="utf-8")
    result = rb.resume_bind(
        tmp_path, harness="claude", harness_session_id=SID, new_pid=os.getpid(),
        claims_root=tmp_path,
    )
    assert result["result"] == "refused"
    assert result["claim_state"] in {"suspect", "stale"}


def test_advice_names_provenance_and_target_start(tmp_path, monkeypatch):
    """A claim-level refusal points the operator at provenance and the successor path.

    Identity matches (same session), but the owner is off-host so the rebind
    refuses at the claim layer, where the advice is attached.
    """
    monkeypatch.setattr(rb, "read_target_manifest", lambda root: _manifest())
    prior = Claim(
        schema_version=1,
        key=KEY,
        holder=HOLDER,
        acquired_at=now_ms(),
        expires_at=now_ms() + 60_000,
        pid=_DEAD_PID,
        host=socket.gethostname(),
        machine_id="00000000-0000-0000-0000-000000000000",
    )
    path = claim_path(KEY, root=tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_claim(prior), encoding="utf-8")
    result = rb.resume_bind(
        tmp_path, harness="claude", harness_session_id=SID, claims_root=tmp_path,
    )
    assert result["result"] == "refused"
    assert "fno backlog provenance" in result["advice"]
    assert "fno do target start" in result["advice"]


# --- manifest_identity (pure) -------------------------------------------


def test_manifest_identity_complete_and_incomplete():
    assert manifest_identity(_manifest()) is not None
    # Missing fno_id -> None (cannot prove identity).
    partial = _manifest()
    partial.pop("fno_id")
    assert manifest_identity(partial) is None
    # A null-valued field -> None.
    partial2 = _manifest()
    partial2["harness_session_id"] = "null"
    assert manifest_identity(partial2) is None
    assert manifest_identity(None) is None
