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
    assert node["status"] == "ready"


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
    assert _read(tmp_graph)[0]["status"] == "ready"


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


def test_unclaim_stale_release_is_holder_verified_toctou(tmp_graph, claims_root, monkeypatch):
    # codex P1: between the stale snapshot and the unlink, another dispatcher
    # reclaims the dead lock with a NEW holder. The release must be holder-
    # verified so it leaves that fresh live lock intact (no two-writer yank).
    _seed(tmp_graph, [_claimed_node()])
    # On-disk reality: a DIFFERENT live holder now owns the lock.
    _acquire("node:ab-1234abcd", "target-session:fresh-live", pid=os.getpid(), root=claims_root)
    # Stale snapshot the verb sees first reports the OLD dead holder.
    import fno.claims.core as cc
    real_status = cc.claim_status
    def stale_snapshot(key, **kw):
        s = dict(real_status(key, **kw))
        if key == "node:ab-1234abcd":
            s.update(state="stale", holder="target-session:dead-old")
        return s
    monkeypatch.setattr("fno.claims.core.claim_status", stale_snapshot)
    result = runner.invoke(app, ["backlog", "unclaim", "ab-1234abcd"])
    assert result.exit_code == 0, result.output
    # The fresh live holder's lock survives (holder mismatch -> release no-ops).
    assert _lock_exists("node:ab-1234abcd", claims_root)
    assert _read(tmp_graph)[0]["session_id"] is None  # graph still cleared


def test_unclaim_refuses_live_foreign_lockfile(tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_claimed_node()])
    # A live holder (this pid) that is NOT us => graph cleared, lockfile kept.
    _acquire("node:ab-1234abcd", "target-session:someone-else", pid=os.getpid(), root=claims_root)
    monkeypatch.setattr(
        "fno.graph.cli._invoking_claim_holder",
        lambda: "target-session:me-not-them",
    )
    result = runner.invoke(app, ["backlog", "unclaim", "ab-1234abcd"])
    assert result.exit_code == 0, result.output
    # Graph claim still cleared...
    assert _read(tmp_graph)[0]["session_id"] is None
    # ...but the live foreign lockfile is left intact, with a force-release hint.
    assert _lock_exists("node:ab-1234abcd", claims_root)
    out = result.output + (result.stderr or "")
    assert "force-release" in out


def _point_session_at(tmp_path: Path, monkeypatch, session_id: str) -> None:
    """Make _invoking_session_id() resolve to `session_id` via a real
    target-state.md, exercising the live repo_root()->Path->resolve_session_id
    path (NOT monkeypatching the helper, so a regression there is caught)."""
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    (tmp_path / ".fno" / "target-state.md").write_text(
        f"---\nsession_id: {session_id}\n---\n", encoding="utf-8"
    )
    # repo_root() returns a str; the helper must wrap it in Path() itself.
    monkeypatch.setattr("fno.graph._intake.repo_root", lambda: str(tmp_path))


def test_invoking_session_id_reads_target_state(tmp_path, monkeypatch):
    # Regression: repo_root() is a str; _invoking_session_id must Path()-wrap it
    # before handing it to resolve_session_id, or it silently returns None.
    import fno.graph.cli as gcli
    _point_session_at(tmp_path, monkeypatch, "sid-123")
    assert gcli._invoking_session_id() == "sid-123"


def test_invoking_claim_holder_prefers_manifest_holder(tmp_path, monkeypatch):
    import fno.graph.cli as gcli

    _point_session_at(tmp_path, monkeypatch, "unique-target-session")
    state = tmp_path / ".fno" / "target-state.md"
    state.write_text(
        state.read_text() + 'target_claim_holder: "target-session:codex-thread"\n'
    )

    assert gcli._invoking_claim_holder() == "target-session:codex-thread"


def test_unclaim_releases_own_live_lockfile(tmp_path, tmp_graph, claims_root, monkeypatch):
    _seed(tmp_graph, [_claimed_node()])
    _acquire("node:ab-1234abcd", "target-session:mine", pid=os.getpid(), root=claims_root)
    _point_session_at(tmp_path, monkeypatch, "mine")  # real helper path, holder = target-session:mine
    result = runner.invoke(app, ["backlog", "unclaim", "ab-1234abcd"])
    assert result.exit_code == 0, result.output
    assert not _lock_exists("node:ab-1234abcd", claims_root)


def test_unclaim_releases_codex_thread_owned_lockfile(
    tmp_path, tmp_graph, claims_root, monkeypatch
):
    _seed(tmp_graph, [_claimed_node()])
    holder = "target-session:019f48e4-codex-thread"
    _acquire("node:ab-1234abcd", holder, pid=os.getpid(), root=claims_root)
    _point_session_at(tmp_path, monkeypatch, "unique-target-session")
    state = tmp_path / ".fno" / "target-state.md"
    state.write_text(state.read_text() + f'target_claim_holder: "{holder}"\n')

    result = runner.invoke(app, ["backlog", "unclaim", "ab-1234abcd"])

    assert result.exit_code == 0, result.output
    assert not _lock_exists("node:ab-1234abcd", claims_root)
