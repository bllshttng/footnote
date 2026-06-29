"""Tests for `fno backlog unclaim` (alias `release`) - one-shot node un-claim.

Covers the graph-claim clear (always), the safe lockfile release (stale or
owned), and the live-foreign refusal (never yank a live peer's claim).
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


@pytest.fixture
def claims_root(tmp_path, monkeypatch) -> Path:
    """Route node: claims into a tmp dir so seeding/asserting locks is hermetic."""
    root = tmp_path / "claims_home"
    root.mkdir()
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(root))
    return root


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _claimed_node(node_id: str = "ab-1234abcd") -> dict:
    return {
        "id": node_id,
        "title": "Claimed thing",
        "slug": "claimed-thing",
        "domain": "code",
        "project": "p",
        "plan_path": "internal/plan.md",  # so the underlying status is `ready`, not `idea`
        "session_id": "20260101T000000Z-1111-aaaaaa",
        "claimed_at": "2026-01-01T00:00:00+00:00",
    }


# -- graph-side clear --------------------------------------------------------


def test_unclaim_reverts_claimed_to_ready(tmp_graph, claims_root):
    _seed(tmp_graph, [_claimed_node()])
    result = runner.invoke(app, ["backlog", "unclaim", "ab-1234abcd"])
    assert result.exit_code == 0, result.output
    node = _read(tmp_graph)[0]
    assert node["session_id"] is None
    assert node["claimed_at"] is None
    assert node["_status"] == "ready"


def test_release_alias_behaves_like_unclaim(tmp_graph, claims_root):
    _seed(tmp_graph, [_claimed_node()])
    result = runner.invoke(app, ["backlog", "release", "ab-1234abcd"])
    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0]["session_id"] is None


def test_unclaim_idempotent_on_ready_node(tmp_graph, claims_root):
    _seed(tmp_graph, [
        {"id": "ab-1234abcd", "title": "Ready", "slug": "ready", "domain": "code",
         "project": "p", "plan_path": "internal/plan.md",
         "session_id": None, "claimed_at": None},
    ])
    result = runner.invoke(app, ["backlog", "unclaim", "ab-1234abcd"])
    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0]["_status"] == "ready"


def test_unclaim_unknown_node_exits_1(tmp_graph, claims_root):
    _seed(tmp_graph, [_claimed_node()])
    result = runner.invoke(app, ["backlog", "unclaim", "ab-deadbeef"])
    assert result.exit_code == 1
    assert "ab-deadbeef" in (result.output + (result.stderr or ""))


# -- lockfile side -----------------------------------------------------------


def _acquire(key: str, holder: str, pid: int, root: Path) -> None:
    # claims_dir(root) appends ".fno/claims"; pass the FNO_CLAIMS_ROOT itself.
    from fno.claims.core import acquire_claim
    acquire_claim(key=key, holder=holder, pid=pid, root=root)


def _lock_exists(key: str, root: Path) -> bool:
    from fno.claims.io import claim_path
    return claim_path(key, root=root).exists()


def test_unclaim_releases_stale_lockfile(tmp_graph, claims_root):
    _seed(tmp_graph, [_claimed_node()])
    # pid that is certainly not alive => classify() returns "stale".
    _acquire("node:ab-1234abcd", "target-session:gone", pid=2_000_000_000, root=claims_root)
    assert _lock_exists("node:ab-1234abcd", claims_root)
    result = runner.invoke(app, ["backlog", "unclaim", "ab-1234abcd"])
    assert result.exit_code == 0, result.output
    assert not _lock_exists("node:ab-1234abcd", claims_root)
    assert _read(tmp_graph)[0]["session_id"] is None


def test_unclaim_refuses_live_foreign_lockfile(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_claimed_node()])
    # A live holder (this pid) that is NOT us => graph cleared, lockfile kept.
    _acquire("node:ab-1234abcd", "target-session:someone-else", pid=os.getpid(), root=claims_root)
    monkeypatch.setattr("fno.graph.cli._invoking_session_id", lambda: "me-not-them")
    result = runner.invoke(app, ["backlog", "unclaim", "ab-1234abcd"])
    assert result.exit_code == 0, result.output
    # Graph claim still cleared...
    assert _read(tmp_graph)[0]["session_id"] is None
    # ...but the live foreign lockfile is left intact, with a force-release hint.
    assert _lock_exists("node:ab-1234abcd", claims_root)
    out = result.output + (result.stderr or "")
    assert "force-release" in out


def test_unclaim_releases_own_live_lockfile(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_claimed_node()])
    _acquire("node:ab-1234abcd", "target-session:mine", pid=os.getpid(), root=claims_root)
    monkeypatch.setattr("fno.graph.cli._invoking_session_id", lambda: "mine")
    result = runner.invoke(app, ["backlog", "unclaim", "ab-1234abcd"])
    assert result.exit_code == 0, result.output
    assert not _lock_exists("node:ab-1234abcd", claims_root)
