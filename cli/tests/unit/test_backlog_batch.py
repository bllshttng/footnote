"""Wave 1 tests for the batch-lane state primitive (cli/src/fno/backlog/batch.py).

A batch is one open branch off origin/main carrying several nodes' commits,
opened as a single PR when it closes. State lives in `.fno/batches/<domain>.json`
(one open batch per domain), flock-guarded and durable across sessions.
"""
from __future__ import annotations

import json
import threading
from pathlib import Path

import pytest

from fno.backlog import batch as B


def _open(root: Path, domain: str = "code", **kw) -> dict:
    kw.setdefault("branch", f"feature/batch-{domain}")
    kw.setdefault("worktree", str(root / "wt"))
    kw.setdefault("max_nodes", 3)
    return B.open_batch(domain=domain, root=root, **kw)


def test_open_creates_batch_file(tmp_path: Path) -> None:
    b = _open(tmp_path)
    assert b["status"] == "open"
    assert b["domain"] == "code"
    assert b["members"] == []
    assert b["batch_id"].startswith("batch-")
    assert B.batch_path("code", tmp_path).exists()


def test_open_twice_same_domain_rejected(tmp_path: Path) -> None:
    _open(tmp_path)
    with pytest.raises(B.BatchExists):
        _open(tmp_path)


def test_join_appends_member_and_returns_batch(tmp_path: Path) -> None:
    _open(tmp_path)
    b = B.join_batch(domain="code", node_id="x-1", summary="did a thing", root=tmp_path)
    assert B.member_ids(b) == ["x-1"]
    b = B.join_batch(domain="code", node_id="x-2", root=tmp_path)
    assert B.member_ids(b) == ["x-1", "x-2"]
    # member summary is persisted
    stored = B.read_batch("code", tmp_path)
    assert stored["members"][0]["summary"] == "did a thing"


def test_join_without_open_batch_raises(tmp_path: Path) -> None:
    with pytest.raises(B.NoOpenBatch):
        B.join_batch(domain="code", node_id="x-1", root=tmp_path)


def test_join_full_batch_raises(tmp_path: Path) -> None:
    _open(tmp_path, max_nodes=2)
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    B.join_batch(domain="code", node_id="x-2", root=tmp_path)
    assert B.is_full(B.read_batch("code", tmp_path))
    with pytest.raises(B.BatchFull):
        B.join_batch(domain="code", node_id="x-3", root=tmp_path)


def test_join_duplicate_node_is_idempotent(tmp_path: Path) -> None:
    _open(tmp_path)
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    b = B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    assert B.member_ids(b) == ["x-1"]


def test_domains_are_isolated(tmp_path: Path) -> None:
    _open(tmp_path, domain="code")
    _open(tmp_path, domain="research")
    B.join_batch(domain="code", node_id="c-1", root=tmp_path)
    B.join_batch(domain="research", node_id="r-1", root=tmp_path)
    assert B.member_ids(B.read_batch("code", tmp_path)) == ["c-1"]
    assert B.member_ids(B.read_batch("research", tmp_path)) == ["r-1"]


def test_close_returns_members_and_marks_closed(tmp_path: Path) -> None:
    _open(tmp_path)
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    closed = B.close_batch(domain="code", pr_url="https://gh/pr/9", root=tmp_path)
    assert closed["status"] == "closed"
    assert closed["pr_url"] == "https://gh/pr/9"
    assert B.member_ids(closed) == ["x-1"]
    # a closed batch is no longer the open one: open() may start fresh
    _open(tmp_path)
    assert B.read_batch("code", tmp_path)["status"] == "open"


def test_close_without_open_batch_raises(tmp_path: Path) -> None:
    with pytest.raises(B.NoOpenBatch):
        B.close_batch(domain="code", root=tmp_path)


def test_abandon_returns_members_to_requeue(tmp_path: Path) -> None:
    _open(tmp_path)
    B.join_batch(domain="code", node_id="x-1", root=tmp_path)
    B.join_batch(domain="code", node_id="x-2", root=tmp_path)
    result = B.abandon_batch(domain="code", root=tmp_path)
    assert result["status"] == "abandoned"
    assert B.member_ids(result) == ["x-1", "x-2"]
    # a new open batch can start after abandon
    _open(tmp_path)
    assert B.read_batch("code", tmp_path)["status"] == "open"


def test_list_batches(tmp_path: Path) -> None:
    assert B.list_batches(tmp_path) == []
    _open(tmp_path, domain="code")
    _open(tmp_path, domain="research")
    domains = {b["domain"] for b in B.list_batches(tmp_path)}
    assert domains == {"code", "research"}


def test_read_missing_batch_returns_none(tmp_path: Path) -> None:
    assert B.read_batch("code", tmp_path) is None


def test_concurrent_joins_do_not_lose_members(tmp_path: Path) -> None:
    """flock serialization: N threads joining must not clobber each other."""
    _open(tmp_path, max_nodes=50)
    errors: list[Exception] = []

    def worker(i: int) -> None:
        try:
            B.join_batch(domain="code", node_id=f"x-{i}", root=tmp_path)
        except Exception as e:  # noqa: BLE001
            errors.append(e)

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(20)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert not errors, errors
    stored = B.read_batch("code", tmp_path)
    assert len(stored["members"]) == 20
    assert {m["node_id"] for m in stored["members"]} == {f"x-{i}" for i in range(20)}


def test_state_file_is_valid_json(tmp_path: Path) -> None:
    _open(tmp_path)
    raw = json.loads(B.batch_path("code", tmp_path).read_text())
    assert raw["status"] == "open"
