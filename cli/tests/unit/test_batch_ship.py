"""Wave 3 tests for per-batch ship (cli/src/fno/backlog/batch.py, x-6cdf).

`ship_batch` opens ONE PR for the open batch's shared branch and records the
shared PR ref (pr_number/pr_url) on every member. It does NOT mark members done
(the PR is not merged yet) - merge-time `fno backlog reconcile` closes each
member by its own pr_number. Any failure to open the PR abandons the batch and
requeues its members as individual PRs (v1 policy).

Filter: `uv run pytest cli/tests/unit/test_batch_ship.py -q`
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from fno.backlog import batch as B


def _open(root: Path, domain: str = "code", **kw) -> dict:
    kw.setdefault("branch", "feature/batch-code")
    kw.setdefault("worktree", str(root / "wt"))
    kw.setdefault("max_nodes", 3)
    return B.open_batch(domain=domain, root=root, **kw)


def _cp(returncode: int = 0, stdout: str = "", stderr: str = "") -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess(args=[], returncode=returncode, stdout=stdout, stderr=stderr)


class FakeGh:
    """A gh runner double: records calls, scripts list/create responses."""

    def __init__(self, *, list_stdout: str = "[]", create: subprocess.CompletedProcess | None = None):
        self.list_stdout = list_stdout
        self.create = create or _cp(0, "https://github.com/o/r/pull/777\n")
        self.calls: list[list[str]] = []

    def __call__(self, cmd, *, cwd=None):
        self.calls.append(cmd)
        if cmd[:3] == ["gh", "pr", "list"]:
            return _cp(0, self.list_stdout)
        if cmd[:3] == ["gh", "pr", "create"]:
            return self.create
        return _cp(0, "")


@pytest.fixture
def graph(tmp_path, monkeypatch):
    """Point ship's graph mutations at a temp graph.json. Returns a writer."""
    path = tmp_path / "graph.json"

    def write(entries):
        path.write_text(json.dumps({"entries": entries}), encoding="utf-8")
        return path

    write([])
    monkeypatch.setattr("fno.paths.graph_json", lambda: path)
    return write


def _member_node(nid: str) -> dict:
    return {
        "id": nid, "title": nid, "domain": "code", "project": "fno",
        "blocked_by": [], "pr_number": None, "pr_url": None, "batch": "batch-xxxx",
        "completed_at": None, "status": "ready",
    }


def _read_graph(path: Path) -> list[dict]:
    return json.loads(path.read_text())["entries"]


def test_ship_no_open_batch_is_noop(tmp_path, graph):
    r = B.ship_batch(domain="code", root=tmp_path, run=FakeGh())
    assert r.action == "noop"


def test_ship_empty_batch_abandoned(tmp_path, graph):
    _open(tmp_path)
    r = B.ship_batch(domain="code", root=tmp_path, run=FakeGh())
    assert r.action == "abandoned"
    assert B.read_batch("code", tmp_path)["status"] == "abandoned"


def test_ship_creates_pr_and_records_member_refs(tmp_path, graph):
    gpath = graph([_member_node("x-1"), _member_node("x-2")])
    _open(tmp_path)
    B.join_batch(domain="code", node_id="x-1", summary="did A", root=tmp_path)
    B.join_batch(domain="code", node_id="x-2", summary="did B", root=tmp_path)

    gh = FakeGh(create=_cp(0, "https://github.com/o/r/pull/777\n"))
    r = B.ship_batch(domain="code", root=tmp_path, run=gh)

    assert r.action == "shipped"
    assert r.pr_number == 777
    assert r.pr_url == "https://github.com/o/r/pull/777"
    # Batch closed with the shared URL.
    closed = B.read_batch("code", tmp_path)
    assert closed["status"] == "closed"
    assert closed["pr_url"] == "https://github.com/o/r/pull/777"
    # Every member carries the SHARED ref, and is NOT marked done.
    by_id = {n["id"]: n for n in _read_graph(gpath)}
    for nid in ("x-1", "x-2"):
        assert by_id[nid]["pr_number"] == 777
        assert by_id[nid]["pr_url"] == "https://github.com/o/r/pull/777"
        assert by_id[nid]["completed_at"] is None  # closed at merge, not ship


def test_ship_reuses_existing_pr(tmp_path, graph):
    graph([_member_node("x-1")])
    _open(tmp_path)
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)

    gh = FakeGh(list_stdout='[{"number": 42, "url": "https://github.com/o/r/pull/42"}]')
    r = B.ship_batch(domain="code", root=tmp_path, run=gh)

    assert r.action == "shipped"
    assert r.pr_number == 42
    # Idempotent: no `gh pr create` was issued.
    assert not any(c[:3] == ["gh", "pr", "create"] for c in gh.calls)


def test_ship_gh_create_failure_abandons_and_requeues(tmp_path, graph):
    gpath = graph([_member_node("x-1"), _member_node("x-2")])
    _open(tmp_path)
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    B.join_batch(domain="code", node_id="x-2", root=tmp_path)

    gh = FakeGh(create=_cp(1, "", "no commits between main and feature/batch-code"))
    r = B.ship_batch(domain="code", root=tmp_path, run=gh)

    assert r.action == "abandoned"
    assert "gh pr create failed" in r.reason
    assert B.read_batch("code", tmp_path)["status"] == "abandoned"
    # Members requeued: batch mark cleared so they resurface in `next`.
    by_id = {n["id"]: n for n in _read_graph(gpath)}
    assert by_id["x-1"]["batch"] is None
    assert by_id["x-2"]["batch"] is None


# ── x-9b87: stale-base guard parity with worker/ship.py ──────────────────────
# The batch lane opens its PR via its own `gh pr create`, so it must run the
# same `check_stale_base` guard the /pr create + worker paths run. On stale it
# routes through the EXISTING abandon path (never `refuse`, which would wedge
# the batch open and re-hit the same stale worktree every daemon tick).


def test_ship_stale_base_abandons_and_skips_create(tmp_path, graph, monkeypatch):
    """AC: HEAD >24h stale vs origin/main -> ShipResult('abandoned'), calls
    _abandon_and_requeue, and does NOT run gh pr create (nor push)."""
    gpath = graph([_member_node("x-1"), _member_node("x-2")])
    _open(tmp_path)
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    B.join_batch(domain="code", node_id="x-2", root=tmp_path)
    monkeypatch.setattr(
        "fno.pr._preflight.check_stale_base",
        lambda *a, **k: (1, "stale base: HEAD is 30h behind origin/main"),
    )
    gh = FakeGh()
    r = B.ship_batch(domain="code", root=tmp_path, run=gh)

    assert r.action == "abandoned"
    assert "stale" in (r.reason or "").lower()
    assert B.read_batch("code", tmp_path)["status"] == "abandoned"
    # The guard precedes create AND push: neither runs on a stale base.
    assert not any(c[:3] == ["gh", "pr", "create"] for c in gh.calls)
    assert not any(c[:2] == ["git", "push"] for c in gh.calls)
    by_id = {n["id"]: n for n in _read_graph(gpath)}
    assert by_id["x-1"]["batch"] is None
    assert by_id["x-2"]["batch"] is None


def test_ship_fresh_base_creates_pr(tmp_path, graph, monkeypatch):
    """AC: fresh worktree (behind-count 0) -> guard passes, PR created as today."""
    graph([_member_node("x-1")])
    _open(tmp_path)
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    monkeypatch.setattr("fno.pr._preflight.check_stale_base", lambda *a, **k: (0, None))

    gh = FakeGh(create=_cp(0, "https://github.com/o/r/pull/501\n"))
    r = B.ship_batch(domain="code", root=tmp_path, run=gh)

    assert r.action == "shipped"
    assert r.pr_number == 501
    assert any(c[:3] == ["gh", "pr", "create"] for c in gh.calls)


def test_ship_stale_guard_failopen_still_creates_pr(tmp_path, graph, monkeypatch):
    """AC: guard fails open (git missing / fetch flake, code 0 + message) ->
    the PR is still created (a skipped guard never blocks a ship)."""
    graph([_member_node("x-1")])
    _open(tmp_path)
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    monkeypatch.setattr(
        "fno.pr._preflight.check_stale_base",
        lambda *a, **k: (0, "could not refresh origin/main; stale-base check skipped"),
    )

    gh = FakeGh(create=_cp(0, "https://github.com/o/r/pull/502\n"))
    r = B.ship_batch(domain="code", root=tmp_path, run=gh)

    assert r.action == "shipped"
    assert r.pr_number == 502


class PrepareGh:
    """Runner double for prepare_batch: scripts `fno backlog get` + `fno worktree ensure`."""

    def __init__(self, *, node: dict, worktree: str = "/wt/batch-code", we_rc: int = 0):
        self.node = node
        self.worktree = worktree
        self.we_rc = we_rc
        self.calls: list[list[str]] = []

    def __call__(self, cmd, *, cwd=None):
        self.calls.append(cmd)
        if cmd[:3] == ["fno-py", "backlog", "get"]:
            return _cp(0, json.dumps(self.node))
        if cmd[:3] == ["fno-py", "worktree", "ensure"]:
            return _cp(self.we_rc, self.worktree if self.we_rc == 0 else "", "boom" if self.we_rc else "")
        return _cp(0, "")


@pytest.fixture
def batching_on(monkeypatch):
    """Force config.batch.enabled True + max_nodes 3 for prepare/ship-closeable."""
    monkeypatch.setattr(B, "_load_batch_enabled", lambda root=None: True)
    monkeypatch.setattr(B, "_config_max_nodes", lambda root: 3)


def test_prepare_solo_when_disabled(tmp_path, monkeypatch):
    monkeypatch.setattr(B, "_load_batch_enabled", lambda root=None: False)
    gh = PrepareGh(node={"id": "x-1", "domain": "code", "size": "S", "priority": "p2"})
    out = B.prepare_batch(node_id="x-1", repo=str(tmp_path), root=tmp_path, run=gh)
    assert out["mode"] == "solo"


def test_prepare_solo_for_size_l(tmp_path, batching_on):
    gh = PrepareGh(node={"id": "x-1", "domain": "code", "size": "L", "priority": "p2"})
    out = B.prepare_batch(node_id="x-1", repo=str(tmp_path), root=tmp_path, run=gh)
    assert out["mode"] == "solo"
    assert "ships alone" in out["reason"]


def test_prepare_start_opens_batch_and_returns_worktree(tmp_path, batching_on):
    gh = PrepareGh(
        node={"id": "x-1", "domain": "code", "size": "S", "priority": "p2"},
        worktree=str(tmp_path / "wt"),
    )
    out = B.prepare_batch(node_id="x-1", repo=str(tmp_path), root=tmp_path, run=gh)
    assert out["mode"] == "batched"
    assert out["domain"] == "code"
    assert out["worktree"] == str(tmp_path / "wt")
    # A batch is now open for the domain with the recorded worktree/branch.
    b = B.read_batch("code", tmp_path)
    assert b["status"] == "open"
    assert b["worktree"] == str(tmp_path / "wt")


def test_prepare_join_reuses_open_batch(tmp_path, batching_on):
    _open(tmp_path, worktree=str(tmp_path / "shared"))
    gh = PrepareGh(node={"id": "x-2", "domain": "code", "size": "S", "priority": "p2"})
    out = B.prepare_batch(node_id="x-2", repo=str(tmp_path), root=tmp_path, run=gh)
    assert out["mode"] == "batched"
    assert out["worktree"] == str(tmp_path / "shared")
    # join path never calls `fno worktree ensure` (reuses the recorded one).
    assert not any(c[:3] == ["fno-py", "worktree", "ensure"] for c in gh.calls)


def test_prepare_solo_when_worktree_ensure_fails(tmp_path, batching_on):
    gh = PrepareGh(node={"id": "x-1", "domain": "code", "size": "S", "priority": "p2"}, we_rc=1)
    out = B.prepare_batch(node_id="x-1", repo=str(tmp_path), root=tmp_path, run=gh)
    assert out["mode"] == "solo"
    assert "worktree ensure failed" in out["reason"]


class CloseableRunner:
    """Runner for ship_closeable: scripts `fno backlog next` + gh pr create."""

    def __init__(self, *, next_node, create_url="https://github.com/o/r/pull/900\n"):
        self.next_node = next_node
        self.create_url = create_url
        self.calls: list[list[str]] = []

    def __call__(self, cmd, *, cwd=None):
        self.calls.append(cmd)
        if cmd[:3] == ["fno-py", "backlog", "next"]:
            return _cp(0, json.dumps(self.next_node) if self.next_node else "null")
        if cmd[:3] == ["gh", "pr", "list"]:
            return _cp(0, "[]")
        if cmd[:3] == ["gh", "pr", "create"]:
            return _cp(0, self.create_url)
        return _cp(0, "")


def test_ship_closeable_ships_on_different_domain_next(tmp_path, graph, batching_on):
    graph([_member_node("x-1")])
    _open(tmp_path, worktree=str(tmp_path / "wt"))
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    # next ready node is a DIFFERENT domain -> should_close -> ship.
    r = CloseableRunner(next_node={"id": "x-9", "domain": "docs"})
    results = B.ship_closeable(project="fno", root=tmp_path, run=r)
    assert len(results) == 1
    assert results[0].action == "shipped"
    assert B.read_batch("code", tmp_path)["status"] == "closed"


def test_ship_closeable_keeps_open_on_same_domain_next(tmp_path, graph, batching_on):
    graph([_member_node("x-1")])
    _open(tmp_path, worktree=str(tmp_path / "wt"))
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    # next ready node is SAME domain and batch not full -> stay open (no ship).
    r = CloseableRunner(next_node={"id": "x-9", "domain": "code"})
    results = B.ship_closeable(project="fno", root=tmp_path, run=r)
    assert results == []
    assert B.read_batch("code", tmp_path)["status"] == "open"


def test_ship_closeable_ships_on_drain(tmp_path, graph, batching_on):
    graph([_member_node("x-1")])
    _open(tmp_path, worktree=str(tmp_path / "wt"))
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    # no next ready node (drain) -> close whatever is open.
    r = CloseableRunner(next_node=None)
    results = B.ship_closeable(project="fno", root=tmp_path, run=r)
    assert len(results) == 1 and results[0].action == "shipped"


class PeekFailRunner:
    """Runner whose `fno backlog next` fails (rc!=0) - a transient peek error."""

    def __call__(self, cmd, *, cwd=None):
        if cmd[:3] == ["fno-py", "backlog", "next"]:
            return _cp(1, "", "graph locked")
        if cmd[:3] == ["gh", "pr", "create"]:
            raise AssertionError("must not ship on a peek error")
        return _cp(0, "")


def test_ship_closeable_skips_tick_on_peek_error(tmp_path, graph, batching_on):
    graph([_member_node("x-1")])
    _open(tmp_path, worktree=str(tmp_path / "wt"))
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    # A transient `fno backlog next` failure must NOT be treated as a drain: the
    # open batch stays open and nothing ships.
    results = B.ship_closeable(project="fno", root=tmp_path, run=PeekFailRunner())
    assert results == []
    assert B.read_batch("code", tmp_path)["status"] == "open"


class PushFailGh(FakeGh):
    """FakeGh whose `git push` fails - the branch could not be published."""

    def __call__(self, cmd, *, cwd=None):
        self.calls.append(cmd)
        if cmd[:2] == ["git", "push"]:
            return _cp(1, "", "remote rejected")
        if cmd[:3] == ["gh", "pr", "create"]:
            raise AssertionError("must not create a PR when push failed")
        if cmd[:3] == ["gh", "pr", "list"]:
            return _cp(0, "[]")
        return _cp(0, "")


def test_ship_batch_push_failure_abandons(tmp_path, graph):
    graph([_member_node("x-1")])
    _open(tmp_path, worktree=str(tmp_path / "wt"))
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    r = B.ship_batch(domain="code", root=tmp_path, run=PushFailGh())
    assert r.action == "abandoned"
    assert "git push failed" in r.reason
    assert B.read_batch("code", tmp_path)["status"] == "abandoned"


def test_ship_closeable_scopes_peek_to_mission(tmp_path, graph, batching_on):
    graph([_member_node("x-1")])
    _open(tmp_path, worktree=str(tmp_path / "wt"))
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    r = CloseableRunner(next_node=None)
    B.ship_closeable(project="fno", root=tmp_path, run=r, mission="m-42")
    next_calls = [c for c in r.calls if c[:3] == ["fno-py", "backlog", "next"]]
    assert next_calls, "expected a next peek"
    assert "--mission" in next_calls[0] and "m-42" in next_calls[0]


def test_abandon_clears_marks_without_releasing_node_claims(tmp_path, graph, monkeypatch):
    """Requeue clears the batch mark but does NOT release node claims: the
    node-claim-release-authority invariant (ab-588326a7) forbids a helper
    subprocess from releasing a node claim. The mark-clear alone requeues (the
    claim TTL only gates latency; deferred to cv-30d898f0)."""
    graph([_member_node("x-1"), _member_node("x-2")])
    import fno.claims.core as _core
    called = []
    monkeypatch.setattr(_core, "force_release_claim", lambda *a, **k: called.append(a))
    B._clear_member_batch_marks(["x-1", "x-2"], root=tmp_path)
    by_id = {n["id"]: n for n in _read_graph(tmp_path / "graph.json")}
    assert by_id["x-1"]["batch"] is None and by_id["x-2"]["batch"] is None
    assert called == [], "must NOT release node claims (ab-588326a7)"


def test_pr_body_lists_members():
    batch = {
        "batch_id": "batch-abcd", "domain": "code",
        "members": [
            {"node_id": "x-1", "summary": "did A"},
            {"node_id": "x-2", "summary": ""},
        ],
    }
    body = B._batch_pr_body(batch)
    assert "batch-abcd" in body
    assert "`x-1` - did A" in body
    assert "`x-2`" in body
