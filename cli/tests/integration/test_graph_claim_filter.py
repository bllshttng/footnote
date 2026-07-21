"""Selection-time node-claim enforcement for `fno graph next` / `ready`.

A node with a LIVE `node:<id>` claim at the global claims root must be
excluded from selection so a second session never picks up a node another
session is actively driving. Stale/expired/released claims must NOT exclude.

The global claims root resolves via Path.home() (i.e. ~/.fno/claims,
mirroring the global ~/.fno/graph.json). Tests isolate by overriding
HOME so Path.home() and the acquire root point at the same tmp dir.

Refs: ab-fcf9cec5 (double-claim of ab-1e86b88e observed across PR #397/#398).
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.claims.core import acquire_claim, release_claim
from fno.claims.io import claim_path, claims_dir, serialize_claim
from fno.claims.types import Claim
from fno.claims.staleness import now_ms

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """Fresh graph.json routed to a temp file; HOME pinned to tmp_path so the
    global claims root (Path.home()/.fno/claims) is isolated too."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    # Pin the global claims root to tmp: clear any inherited override so
    # global_claims_root() falls through to $HOME (which we pin here), and the
    # acquire root (tmp_path) and the selection filter resolve to the same dir.
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path))
    return g


# Recent so the G1 stale-ready guard never quarantines these fixtures.
_RECENT_CREATED = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat()


def _two_ready_entries():
    return [
        {"id": "ab-aaaaaaaa", "title": "A", "status": "ready", "priority": "p2",
         "created_at": _RECENT_CREATED, "project": "p", "blocked_by": [], "plan_path": "a.md"},
        {"id": "ab-bbbbbbbb", "title": "B", "status": "ready", "priority": "p2",
         "created_at": _RECENT_CREATED, "project": "p", "blocked_by": [], "plan_path": "b.md"},
    ]


def _invoke(*args):
    return runner.invoke(app, list(args), catch_exceptions=False)


def test_next_skips_live_claimed_node(tmp_graph, tmp_path):
    """A live TTL claim on ab-aaaaaaaa makes `graph next` pick ab-bbbbbbbb."""
    tmp_graph.write_text(json.dumps({"entries": _two_ready_entries()}) + "\n")
    # TTL claim is live regardless of the acquiring process's liveness.
    acquire_claim(
        key="node:ab-aaaaaaaa",
        holder="target-session:other",
        ttl_ms=3_600_000,
        root=tmp_path,
    )
    r = _invoke("graph", "next", "--all")
    out = json.loads(r.stdout)
    assert out is not None, r.stdout
    assert out["id"] == "ab-bbbbbbbb"


def test_ready_excludes_live_claimed_node(tmp_graph, tmp_path):
    """`graph ready` omits a live-claimed node from the listing."""
    tmp_graph.write_text(json.dumps({"entries": _two_ready_entries()}) + "\n")
    acquire_claim(
        key="node:ab-aaaaaaaa",
        holder="target-session:other",
        ttl_ms=3_600_000,
        root=tmp_path,
    )
    r = _invoke("graph", "ready", "--all")
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert "ab-aaaaaaaa" not in ids
    assert "ab-bbbbbbbb" in ids


def test_released_claim_does_not_block(tmp_graph, tmp_path):
    """After release the node is selectable again (only LIVE claims filter)."""
    tmp_graph.write_text(json.dumps({"entries": _two_ready_entries()}) + "\n")
    acquire_claim(key="node:ab-aaaaaaaa", holder="h", ttl_ms=3_600_000, root=tmp_path)
    release_claim(key="node:ab-aaaaaaaa", holder="h", root=tmp_path)
    r = _invoke("graph", "ready", "--all")
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert "ab-aaaaaaaa" in ids


def test_expired_claim_does_not_block(tmp_graph, tmp_path):
    """A stale (expired TTL) claim must not exclude its node from selection."""
    tmp_graph.write_text(json.dumps({"entries": _two_ready_entries()}) + "\n")
    # Write an already-expired claim file directly (acquire validates ttl bounds).
    cdir = claims_dir(tmp_path)
    cdir.mkdir(parents=True, exist_ok=True)
    past = now_ms() - 1000
    expired = Claim(
        key="node:ab-aaaaaaaa",
        holder="dead",
        acquired_at=past - 60_000,
        expires_at=past,
        pid=999999,
        host="somehost",
        reason=None,
        metadata={},
    )
    claim_path("node:ab-aaaaaaaa", root=tmp_path).write_text(serialize_claim(expired))
    r = _invoke("graph", "ready", "--all")
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert "ab-aaaaaaaa" in ids, "expired claim should not block selection"


def test_no_claims_directory_is_graceful(tmp_graph, tmp_path):
    """Absent claims dir: selection behaves exactly as before (no crash)."""
    tmp_graph.write_text(json.dumps({"entries": _two_ready_entries()}) + "\n")
    r = _invoke("graph", "next", "--all")
    out = json.loads(r.stdout)
    assert out is not None
    assert out["id"] in {"ab-aaaaaaaa", "ab-bbbbbbbb"}
