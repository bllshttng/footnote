"""Node closure releases its claim; reap settles node-aware (x-94f8).

Measured defect: a session that finished one node and moved to the next kept
a LIVE claim on the dead one for 16 hours. Closing a node had no release on
any path, and the reaper could only ask "is the holder alive?" - never "is
the holder still on THIS node?". These tests pin the two halves: the store's
closure-release hook (every closure path funnels through it) and the
settlement reading in the single reap decision, plus the board queue and the
graph lock mirror that made the leak read as a stall.
"""
from __future__ import annotations

import json
import os
import socket
from pathlib import Path

from fno.claims.cli import RosterReading, _node_settlement
from fno.claims.core import reap_dead_claims, sweep_verdict
from fno.claims.io import claim_path, claims_dir, serialize_claim
from fno.claims.staleness import now_ms
from fno.claims.types import Claim
from fno.graph.store import locked_mutate_graph, read_graph, release_node_claim_at_closure
from fno.king.board import BoardInputs, SourceRead, build_board


HOLDER = "target-session:sid-a"
MACHINE_ID = socket.gethostname()


def _dead_pid() -> int:
    dead = 999_999
    import psutil

    while psutil.pid_exists(dead):
        dead += 1
    return dead


def _write_claim(key: str, *, holder: str, pid: int, expires_at_ms: int, root: Path) -> Path:
    claim = Claim(
        key=key,
        holder=holder,
        acquired_at=now_ms() - 7_200_000,
        expires_at=expires_at_ms,
        pid=pid,
        host=socket.gethostname(),
    )
    path = claim_path(key, root=root)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(serialize_claim(claim))
    return path


def _make_graph(tmp_path: Path, entries: list[dict]) -> Path:
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": entries}) + "\n")
    return p


# ---------------------------------------------------------------------------
# Task 1: the store's closure-release hook
# ---------------------------------------------------------------------------


class TestClosureReleaseHook:
    def _graph_with_claimed_node(self, tmp_path, monkeypatch) -> tuple[Path, Path]:
        global_root = tmp_path / "global"
        monkeypatch.setenv("FNO_CLAIMS_ROOT", str(global_root))
        graph = _make_graph(
            tmp_path,
            [
                {
                    "id": "x-doen",
                    "title": "claimed node",
                    "status": "in_progress",
                    "locked_by": HOLDER,
                    "claimed_at": "2026-08-21T00:00:00Z",
                    "session_id": HOLDER,
                }
            ],
        )
        # The release fires only when the mutated graph IS the process's
        # configured graph (a scratch graph closing a same-id node must not
        # release the configured fleet's claim).
        monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
        _write_claim(
            "node:x-doen",
            holder=HOLDER,
            pid=os.getpid(),
            expires_at_ms=now_ms() + 3_600_000,
            root=global_root,
        )
        return graph, global_root

    def test_scratch_graph_closure_does_not_release(self, tmp_path, monkeypatch):
        """A non-configured graph (tests, capture flows) owns no global claim:
        its closure clears only its own mirror."""
        graph, global_root = self._graph_with_claimed_node(tmp_path, monkeypatch)
        monkeypatch.setattr(
            "fno.paths.graph_json", lambda: tmp_path / "the-configured-one.json"
        )

        def _close(entries):
            for e in entries:
                if e["id"] == "x-doen":
                    e["completed_at"] = "2026-08-21T03:16:00Z"
            return entries

        locked_mutate_graph(graph, _close)
        assert read_graph(graph)[0]["status"] == "done"
        assert read_graph(graph)[0]["locked_by"] is None
        assert claim_path("node:x-doen", root=global_root).exists()

    def test_done_releases_claim_and_clears_mirror(self, tmp_path, monkeypatch):
        graph, global_root = self._graph_with_claimed_node(tmp_path, monkeypatch)

        def _close(entries):
            for e in entries:
                if e["id"] == "x-doen":
                    e["completed_at"] = "2026-08-21T03:16:00Z"
            return entries

        locked_mutate_graph(graph, _close)

        out = read_graph(graph)[0]
        assert out["status"] == "done"
        assert out["locked_by"] is None
        assert out["claimed_at"] is None
        # session_id on a done node is work/cost provenance, not a lock.
        assert out["session_id"] == HOLDER
        assert not claim_path("node:x-doen", root=global_root).exists()
        expired = list((claims_dir(global_root) / ".expired").glob("*.lock"))
        assert expired, "the released claim must be archived, not vanished"

    def test_supersede_releases_claim_and_clears_mirror(self, tmp_path, monkeypatch):
        graph, global_root = self._graph_with_claimed_node(tmp_path, monkeypatch)

        def _supersede(entries):
            for e in entries:
                if e["id"] == "x-doen":
                    e["superseded_by"] = "x-other"
            return entries

        locked_mutate_graph(graph, _supersede)

        out = read_graph(graph)[0]
        assert out["status"] == "superseded"
        assert out["locked_by"] is None
        assert not claim_path("node:x-doen", root=global_root).exists()

    def test_no_terminal_transition_no_release(self, tmp_path, monkeypatch):
        """A claim planted on an ALREADY-terminal node survives an unrelated
        mutation: the hook fires on the transition, not on terminal-ness, so
        it never turns into a per-mutation sweep."""
        graph, global_root = self._graph_with_claimed_node(tmp_path, monkeypatch)

        def _already_done(entries):
            for e in entries:
                if e["id"] == "x-doen":
                    e["completed_at"] = "2026-08-21T03:16:00Z"
            return entries

        locked_mutate_graph(graph, _already_done)
        assert not claim_path("node:x-doen", root=global_root).exists()

        # Replant (the pre-fix leak shape) and mutate again: kept.
        _write_claim(
            "node:x-doen",
            holder=HOLDER,
            pid=os.getpid(),
            expires_at_ms=now_ms() + 3_600_000,
            root=global_root,
        )

        def _retitle(entries):
            for e in entries:
                if e["id"] == "x-doen":
                    e["title"] = "retitled"
            return entries

        locked_mutate_graph(graph, _retitle)
        assert claim_path("node:x-doen", root=global_root).exists()

    def test_a_broken_claims_store_never_fails_the_mutation(self, tmp_path, monkeypatch):
        graph, _ = self._graph_with_claimed_node(tmp_path, monkeypatch)

        def _boom(*_a, **_k):
            raise RuntimeError("claims on fire")

        monkeypatch.setattr("fno.claims.core.force_release_claim", _boom)

        def _close(entries):
            for e in entries:
                if e["id"] == "x-doen":
                    e["completed_at"] = "2026-08-21T03:16:00Z"
            return entries

        # The closure lands; the release failure is a stderr line, not an exit.
        locked_mutate_graph(graph, _close)
        assert read_graph(graph)[0]["status"] == "done"


# ---------------------------------------------------------------------------
# Task 2: the helper itself, and the no-claim case
# ---------------------------------------------------------------------------


def test_release_at_closure_is_a_noop_without_a_claim_file(tmp_path, monkeypatch, capsys):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "empty"))
    release_node_claim_at_closure("x-never", rung="done")
    assert capsys.readouterr().err == ""


# ---------------------------------------------------------------------------
# Task 3: the node-aware settlement
# ---------------------------------------------------------------------------


def _reading(workers_by_node: dict, *, consulted: bool = True) -> RosterReading:
    rows_by_session = {
        row["row_id"]: row
        for rows in workers_by_node.values()
        for row in rows
    }
    return RosterReading(
        consulted=consulted,
        rows_scanned=sum(len(v) for v in workers_by_node.values()) or 1,
        workers_by_node=workers_by_node,
        rows_by_session=rows_by_session,
    )


def _row(session_id: str = "sid-a") -> dict:
    return {"name": "w", "state": "working", "cwd": "/w", "row_id": session_id}


def _expired_live_claim(key: str = "node:x-gone") -> Claim:
    """The specimen shape: TTL long past, holder pid alive -> classify LIVE."""
    return Claim(
        key=key,
        holder="target-session:sid-a",
        acquired_at=now_ms() - 7_200_000,
        expires_at=now_ms() - 3_600_000,
        pid=os.getpid(),
        host=socket.gethostname(),
    )


class TestNodeSettlement:
    def test_terminal_node_settles_a_live_expired_claim(self, tmp_path, monkeypatch):
        """The healing read: a node the graph closed has no legitimate holder,
        whatever the pid table says."""
        graph = _make_graph(tmp_path, [{"id": "x-gone", "status": "done", "completed_at": "2026-08-21T03:16:00Z"}])
        monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
        verdict, bucket = sweep_verdict(
            _expired_live_claim(), node_settlement=_node_settlement(_reading({}))
        )
        assert verdict is True and bucket == ""

    def test_legacy_defer_sentinel_is_not_terminal(self, tmp_path, monkeypatch):
        """A pre-migration row carries deferral inside completed_at; deferral
        is a RETURNABLE rung, and settling a live claim on it would hand the
        node to a second worker mid-deferral."""
        graph = _make_graph(
            tmp_path,
            [{"id": "x-gone", "completed_at": "deferred:2026-08-01T00:00:00Z"}],
        )
        monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
        settlement = _node_settlement(_reading({"x-gone": [_row()]}))
        # The holder is on THIS node and the lease reads unexpired to the
        # roster arm's gate; nothing here may settle.
        claim = Claim(
            key="node:x-gone",
            holder="target-session:sid-a",
            acquired_at=now_ms(),
            expires_at=now_ms() + 3_600_000,
            pid=os.getpid(),
            host=socket.gethostname(),
        )
        assert settlement(claim, now=now_ms()) is None

    def test_holder_on_a_different_node_settles_an_expired_lease(self):
        settlement = _node_settlement(_reading({"x-other": [_row()]}))
        assert settlement(_expired_live_claim(), now=now_ms()) is True

    def test_holder_on_this_node_is_not_settled(self):
        settlement = _node_settlement(_reading({"x-gone": [_row()]}))
        assert settlement(_expired_live_claim(), now=now_ms()) is None

    def test_row_absent_is_not_settled(self):
        """Not-found is not gone (the doctrine the probe lives by)."""
        settlement = _node_settlement(_reading({"x-other": [_row("someone-else")]}))
        assert settlement(_expired_live_claim(), now=now_ms()) is None

    def test_unexpired_lease_is_never_settled(self):
        settlement = _node_settlement(_reading({"x-other": [_row()]}))
        claim = Claim(
            key="node:x-gone",
            holder="target-session:sid-a",
            acquired_at=now_ms(),
            expires_at=now_ms() + 3_600_000,
            pid=os.getpid(),
            host=socket.gethostname(),
        )
        assert settlement(claim, now=now_ms()) is None

    def test_roster_not_consulted_is_not_settled(self):
        settlement = _node_settlement(_reading({}, consulted=False))
        assert settlement(_expired_live_claim(), now=now_ms()) is None

    def test_unreadable_graph_is_not_settled(self, tmp_path, monkeypatch):
        monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "nope.json")
        settlement = _node_settlement(_reading({"x-other": [_row()]}))
        # Graph unreadable -> None; the different-node arm still answers.
        assert settlement(_expired_live_claim(), now=now_ms()) is True

    def test_settlement_true_reaps_through_the_sweep(self, tmp_path, monkeypatch):
        """The settlement reaches reap_dead_claims, not just the predicate."""
        # Pin the settlement's graph read away from the operator's real graph.
        monkeypatch.setattr(
            "fno.paths.graph_json", lambda: tmp_path / "not-a-graph.json"
        )
        _write_claim(
            "node:x-gone",
            holder=HOLDER,
            pid=os.getpid(),
            expires_at_ms=now_ms() - 3_600_000,
            root=tmp_path,
        )
        summary = reap_dead_claims(
            roots=[tmp_path],
            apply=True,
            node_settlement=_node_settlement(_reading({"x-other": [_row()]})),
        )
        assert summary["reaped"] == 1
        assert summary["kept_live"] == 0

    def test_a_broken_settlement_keeps_the_claim(self, tmp_path):
        _write_claim(
            "node:x-gone",
            holder=HOLDER,
            pid=_dead_pid(),
            expires_at_ms=now_ms() + 3_600_000,
            root=tmp_path,
        )

        def _boom(_claim, now=None):
            raise RuntimeError("settlement on fire")

        # Falls through to liveness: dead pid + unexpired TTL = suspect, kept.
        verdict, bucket = sweep_verdict(
            Claim(
                key="node:x-gone",
                holder=HOLDER,
                acquired_at=now_ms(),
                expires_at=now_ms() + 3_600_000,
                pid=_dead_pid(),
                host=socket.gethostname(),
            ),
            node_settlement=_boom,
        )
        assert verdict is False


# ---------------------------------------------------------------------------
# Task 4: the king board never reports a done node as a stalled holder
# ---------------------------------------------------------------------------


def _held_inputs(node: dict, holder: str) -> BoardInputs:
    return BoardInputs(
        ready=SourceRead(payload=[]),
        claims=SourceRead(
            payload=[{"key": f"node:{node['id']}", "state": "live", "holder": holder}]
        ),
        claimed_nodes=SourceRead(payload=[node]),
        holder_activity={holder: {"state": "stalled", "age_s": 9000}},
        prs=SourceRead(payload=[]),
        questions=SourceRead(payload=[]),
        needs=SourceRead(payload=[]),
        lane=SourceRead(payload=[]),
    )


def _stalled_ids(board) -> list:
    for q in board["queues"]:
        if q["name"] == "stalled_holder":
            return [r["id"] for r in q["rows"]]
    raise AssertionError("no stalled_holder queue")


def test_stalled_holder_excludes_done_nodes():
    node = {
        "id": "x-doen",
        "priority": "p0",
        "status": "done",
        "completed_at": "2026-08-21T03:16:00Z",
    }
    board = build_board(_held_inputs(node, HOLDER))
    assert _stalled_ids(board) == []


def test_stalled_holder_still_names_a_live_open_node():
    node = {"id": "x-open", "priority": "p0", "status": "in_progress"}
    board = build_board(_held_inputs(node, HOLDER))
    assert _stalled_ids(board) == ["x-open"]


# ---------------------------------------------------------------------------
# Task 5: reap clears the graph lock mirror
# ---------------------------------------------------------------------------


class TestReapMirrorClear:
    def _dead_claim_and_graph(self, tmp_path, monkeypatch):
        claims_root = tmp_path / "claims-home"
        monkeypatch.setenv("FNO_CLAIMS_ROOT", str(claims_root))
        graph = _make_graph(
            tmp_path,
            [
                {
                    "id": "x-gone",
                    "title": "reaped worker's node",
                    "status": "in_progress",
                    "locked_by": HOLDER,
                    "claimed_at": "2026-08-20T00:00:00Z",
                    "session_id": HOLDER,
                }
            ],
        )
        monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
        _write_claim(
            "node:x-gone",
            holder=HOLDER,
            pid=_dead_pid(),
            expires_at_ms=now_ms() - 3_600_000,
            root=claims_root,
        )
        return graph, claims_root

    def test_apply_clears_the_mirror(self, tmp_path, monkeypatch):
        graph, _root = self._dead_claim_and_graph(tmp_path, monkeypatch)
        # No roots=: the DEFAULT sweep, which is the only sweep that owns
        # this process's graph.
        summary = reap_dead_claims(apply=True)
        assert summary["reaped"] == 1
        assert summary["lock_mirror_cleared"] == 1
        out = read_graph(graph)[0]
        assert out["locked_by"] is None
        assert out["claimed_at"] is None

    def test_explicit_root_sweep_never_touches_the_graph(self, tmp_path, monkeypatch):
        """--root sweeps someone else's claims tree; the mirror belongs to
        this graph and stays."""
        graph, claims_root = self._dead_claim_and_graph(tmp_path, monkeypatch)
        summary = reap_dead_claims(roots=[claims_root], apply=True)
        assert summary["reaped"] == 1
        assert summary["lock_mirror_cleared"] == 0
        out = read_graph(graph)[0]
        assert out["locked_by"] == HOLDER

    def test_dry_run_never_touches_the_graph(self, tmp_path, monkeypatch):
        graph, _root = self._dead_claim_and_graph(tmp_path, monkeypatch)
        summary = reap_dead_claims(apply=False)
        assert summary["would_reap"] == 1
        assert summary["lock_mirror_cleared"] == 0
        out = read_graph(graph)[0]
        assert out["locked_by"] == HOLDER
