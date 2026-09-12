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
from fno.claims.types import Claim, now_ms

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
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # Seam readers resolve fno.paths.graph_json at call time; pin the
    # resolver to the same hermetic file (module-attr pins do not reach it).
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
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
    r = _invoke("backlog", "next", "--all")
    out = json.loads(r.stdout)
    assert out is not None, r.stdout
    assert out["id"] == "ab-bbbbbbbb"


def test_next_reads_each_liveness_source_once(tmp_graph, tmp_path, monkeypatch):
    """One selection, one claim verdict scan, one roster read.

    Three layers used to rescan live occupancy inside a single `backlog next`
    and got the same answer each time; the scans are what the command costs.
    """
    entries = _two_ready_entries() + [
        {"id": "ab-cccccccc", "title": "C", "status": "ready", "priority": "p2",
         "created_at": _RECENT_CREATED, "project": "p", "blocked_by": [], "plan_path": "c.md"},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    acquire_claim(
        key="node:ab-aaaaaaaa",
        holder="target-session:other",
        ttl_ms=3_600_000,
        root=tmp_path,
    )

    import fno.graph.statuses as statuses

    calls = {"claimed": 0, "worked": 0}
    real_claimed = statuses.live_claimed_node_ids

    def counting_claimed(**kwargs):
        calls["claimed"] += 1
        return real_claimed(**kwargs)

    def counting_worked(**_kwargs):
        calls["worked"] += 1
        return {"ab-cccccccc": ["bp-worker"]}

    monkeypatch.setattr(statuses, "live_claimed_node_ids", counting_claimed)
    monkeypatch.setattr("fno.graph.cli._live_claimed_node_ids", counting_claimed)
    monkeypatch.setattr(statuses, "live_worked_node_ids", counting_worked)

    result = _invoke("backlog", "next", "--all")

    assert result.exit_code == 0, result.output
    # The claimed node and the worked node are both occupied; only C is free.
    assert json.loads(result.stdout)["id"] == "ab-bbbbbbbb"
    assert calls == {"claimed": 1, "worked": 1}, result.output


def test_next_refuses_when_worked_evidence_is_unreadable(tmp_graph, monkeypatch):
    """An unreadable roster refuses selection; it never reads as unoccupied."""
    entries = _two_ready_entries()
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    def unavailable(**_kwargs):
        raise RuntimeError("roster timeout")

    monkeypatch.setattr("fno.graph.statuses.live_worked_node_ids", unavailable)

    result = _invoke("backlog", "next", "--all")

    assert result.exit_code == 1, result.output
    assert "roster timeout" in result.output
    assert "selection refused" in result.output
    assert '"id"' not in result.output
    assert json.loads(tmp_graph.read_text())["entries"] == entries


def test_next_refuses_when_the_graph_is_unreadable(tmp_graph, monkeypatch):
    """A corrupt graph refuses selection; it never selects over zero rows.

    `read_graph` swallows corruption and answers no rows, and a selection over
    no rows prints `null`, which `advance` reads as the benign `no-work` skip.
    """
    entries = _two_ready_entries()
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    def unreadable(*_args, **_kwargs):
        raise RuntimeError("graph.json is corrupt")

    monkeypatch.setattr("fno.graph.store.read_graph_strict", unreadable)

    result = _invoke("backlog", "next", "--all")

    assert result.exit_code == 1, result.output
    assert "graph unreadable" in result.output
    assert "graph.json is corrupt" in result.output
    assert '"id"' not in result.output


def test_ready_excludes_live_claimed_node(tmp_graph, tmp_path):
    """`graph ready` omits a live-claimed node from the listing."""
    tmp_graph.write_text(json.dumps({"entries": _two_ready_entries()}) + "\n")
    acquire_claim(
        key="node:ab-aaaaaaaa",
        holder="target-session:other",
        ttl_ms=3_600_000,
        root=tmp_path,
    )
    r = _invoke("backlog", "ready", "--all")
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert "ab-aaaaaaaa" not in ids
    assert "ab-bbbbbbbb" in ids


def test_next_prefers_sibling_of_live_claimed_epic(tmp_graph, tmp_path):
    entries = [
        {"id": "ab-epic001", "title": "Active epic", "type": "epic",
         "status": "ready", "priority": "p2", "created_at": "2026-02-01",
         "project": "p", "blocked_by": []},
        {"id": "ab-epic002", "title": "Idle epic", "type": "epic",
         "status": "ready", "priority": "p2", "created_at": "2026-01-01",
         "project": "p", "blocked_by": []},
        {"id": "ab-claimed1", "title": "Claimed child", "status": "ready",
         "parent": "ab-epic001", "priority": "p2", "created_at": _RECENT_CREATED,
         "project": "p", "blocked_by": [], "plan_path": "claimed.md"},
        {"id": "ab-sibling1", "title": "Active sibling", "status": "ready",
         "parent": "ab-epic001", "priority": "p2", "created_at": _RECENT_CREATED,
         "project": "p", "blocked_by": [], "plan_path": "sibling.md"},
        {"id": "ab-idlekid1", "title": "Idle child", "status": "ready",
         "parent": "ab-epic002", "priority": "p2", "created_at": _RECENT_CREATED,
         "project": "p", "blocked_by": [], "plan_path": "idle.md"},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    acquire_claim(
        key="node:ab-claimed1",
        holder="target-session:other",
        ttl_ms=3_600_000,
        root=tmp_path,
    )

    result = _invoke("backlog", "next", "--all")

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["id"] == "ab-sibling1"


def test_parallel_next_draw_holds_unique_nodes(tmp_graph, tmp_path):
    """Each serialized lane claims its pick before the next lane selects."""
    max_lanes = 3
    entries = [
        {
            "id": f"ab-0000000{i}", "title": f"Node {i}", "status": "ready",
            "priority": "p1", "created_at": _RECENT_CREATED, "project": "p",
            "blocked_by": [], "plan_path": f"{i}.md",
        }
        for i in range(1, max_lanes + 2)
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    selected: list[str] = []

    for lane in range(max_lanes):
        out = json.loads(_invoke("backlog", "next", "--all").stdout)
        assert out["id"] not in selected
        selected.append(out["id"])
        acquire_claim(
            key=f"node:{out['id']}",
            holder=f"target-session:lane-{lane}",
            ttl_ms=3_600_000,
            root=tmp_path,
        )

    assert len(set(selected)) == max_lanes


def test_rank_uses_stored_status_board_lane(tmp_graph, tmp_path):
    entries = [
        {"id": "ab-claimed1", "title": "Claimed", "status": "ready",
         "priority": "p3", "project": "p"},
        {"id": "ab-anchor1", "title": "Now anchor", "status": "ready",
         "priority": "p3", "project": "p", "rank": 5.0},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    acquire_claim(
        key="node:ab-claimed1",
        holder="target-session:other",
        ttl_ms=3_600_000,
        root=tmp_path,
    )

    result = _invoke(
        "backlog", "rank", "ab-claimed1", "--before", "ab-anchor1"
    )

    assert result.exit_code == 0, result.output
    assert "Later/p" in result.output
    persisted = {
        entry["id"]: entry
        for entry in json.loads(tmp_graph.read_text())["entries"]
    }
    assert persisted["ab-anchor1"]["rank"] == 5.0
    assert persisted["ab-claimed1"]["rank"] < persisted["ab-anchor1"]["rank"]


def test_rank_does_not_need_live_claim_state(tmp_graph):
    entries = [
        {"id": "ab-target01", "title": "Target", "status": "ready",
         "priority": "p3", "project": "p"},
        {"id": "ab-anchor01", "title": "Anchor", "status": "ready",
         "priority": "p3", "project": "p", "rank": 5.0},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    result = _invoke(
        "backlog", "rank", "ab-target01", "--before", "ab-anchor01"
    )

    assert result.exit_code == 0, result.output
    persisted = {
        entry["id"]: entry
        for entry in json.loads(tmp_graph.read_text())["entries"]
    }
    assert persisted["ab-target01"]["rank"] < persisted["ab-anchor01"]["rank"]


def test_released_claim_does_not_block(tmp_graph, tmp_path):
    """After release the node is selectable again (only LIVE claims filter)."""
    tmp_graph.write_text(json.dumps({"entries": _two_ready_entries()}) + "\n")
    acquire_claim(key="node:ab-aaaaaaaa", holder="h", ttl_ms=3_600_000, root=tmp_path)
    release_claim(key="node:ab-aaaaaaaa", holder="h", root=tmp_path)
    r = _invoke("backlog", "ready", "--all")
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert "ab-aaaaaaaa" in ids


@pytest.mark.parametrize("command", [("next", "--all"), ("ready", "--all")])
def test_dispatch_selection_refuses_when_live_claim_state_is_unavailable(
    tmp_graph, tmp_path, monkeypatch, command
):
    entries = _two_ready_entries()
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    locked = None
    if command[0] == "ready":
        # The ready leg enforces claim liveness inside the keeper, so the
        # refusal is driven through the env the spawned keeper inherits:
        # a claims root that exists but cannot be read is UNKNOWN state,
        # which must refuse, never read as "nothing is claimed".
        locked = tmp_path / "claims-locked"
        locked.mkdir()
        locked.chmod(0o000)
        monkeypatch.setenv("FNO_CLAIMS_ROOT", str(locked))
    else:

        def unavailable(*args, **kwargs):
            raise OSError("claims unavailable")

        monkeypatch.setattr("fno.graph.cli._live_claimed_node_ids", unavailable)

    try:
        result = _invoke("backlog", *command)

        assert result.exit_code == 1
        assert "live claim state is unavailable" in result.output
        assert json.loads(tmp_graph.read_text())["entries"] == entries
    finally:
        # A mode-000 dir left behind makes the next run's rm_rf of this tree
        # fail with Errno 66 ("Directory not empty") - no process needed.
        if locked is not None:
            locked.chmod(0o700)


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
    r = _invoke("backlog", "ready", "--all")
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert "ab-aaaaaaaa" in ids, "expired claim should not block selection"


def test_no_claims_directory_is_graceful(tmp_graph, tmp_path):
    """Absent claims dir: selection behaves exactly as before (no crash)."""
    tmp_graph.write_text(json.dumps({"entries": _two_ready_entries()}) + "\n")
    r = _invoke("backlog", "next", "--all")
    out = json.loads(r.stdout)
    assert out is not None
    assert out["id"] in {"ab-aaaaaaaa", "ab-bbbbbbbb"}
