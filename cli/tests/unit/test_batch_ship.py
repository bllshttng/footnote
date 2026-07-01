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
        "completed_at": None, "_status": "ready",
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
