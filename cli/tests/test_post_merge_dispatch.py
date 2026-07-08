"""Tests for post-merge-ritual auto-dispatch + merge-SHA threading (x-47be, wave 2).

Covers task 2.1 (dispatch), 2.2 (at-most-one dedup keyed on merge SHA), and 2.3
(concurrency + location + SHA threading). The spawn seam is injected so no real
`fno agents spawn` fires.
"""
from __future__ import annotations

from pathlib import Path

from fno.graph._reconcile import (
    MergeDriftRecord,
    PrMergeState,
    dispatch_post_merge_ritual,
    query_pr_merge_state,
    scan_merge_drift,
)


class _Spawn:
    def __init__(self, short_id="abc123", fail=False):
        self.short_id = short_id
        self.fail = fail
        self.calls: list[tuple[int, str]] = []

    def __call__(self, pr_number: int, cwd: str) -> str:
        self.calls.append((pr_number, cwd))
        if self.fail:
            raise RuntimeError("spawn boom")
        return self.short_id


# --- task 2.1: dispatch gating -------------------------------------------


def test_disabled_never_spawns(tmp_path):
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="sha1", auto_run=False, canonical_root=tmp_path, spawn=spawn
    )
    assert res.outcome == "disabled"
    assert spawn.calls == []


def test_dispatch_spawns_once_and_marks(tmp_path):
    spawn = _Spawn(short_id="xy")
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaA", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert res.outcome == "dispatched"
    assert res.short_id == "xy"
    assert spawn.calls == [(7, str(tmp_path))]
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaA").exists()


# --- task 2.2 / AC1-FR: at-most-one per merge SHA ------------------------


def test_second_dispatch_same_sha_is_noop(tmp_path):
    spawn = _Spawn()
    first = dispatch_post_merge_ritual(
        7, dedup_key="shaB", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    second = dispatch_post_merge_ritual(
        7, dedup_key="shaB", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert first.outcome == "dispatched"
    assert second.outcome == "already-dispatched"
    assert len(spawn.calls) == 1  # exactly one worker for the merge SHA


def test_distinct_shas_each_dispatch(tmp_path):
    spawn = _Spawn()
    dispatch_post_merge_ritual(
        7, dedup_key="shaC", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    dispatch_post_merge_ritual(
        8, dedup_key="shaD", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert len(spawn.calls) == 2


def test_spawn_failure_drops_marker_for_retry(tmp_path):
    spawn = _Spawn(fail=True)
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaE", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert res.outcome == "spawn-failed"
    # marker removed so the next reconcile retries
    assert not (tmp_path / ".fno" / "post-merge-dispatched" / "shaE").exists()
    # a retry now succeeds
    ok = _Spawn()
    res2 = dispatch_post_merge_ritual(
        7, dedup_key="shaE", auto_run=True, canonical_root=tmp_path, spawn=ok
    )
    assert res2.outcome == "dispatched"
    assert len(ok.calls) == 1


def test_missing_sha_falls_back_to_pr_key(tmp_path):
    spawn = _Spawn()
    dispatch_post_merge_ritual(
        42, dedup_key=None, auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "pr-42").exists()


# --- task 2.3: location - dispatch marker lands under canonical ----------


def test_marker_under_provided_canonical_not_cwd(tmp_path):
    """The dispatch always targets the canonical root it is given, never the
    caller's cwd (a worktree run must still mark the canonical)."""
    canonical = tmp_path / "canonical"
    canonical.mkdir()
    spawn = _Spawn()
    dispatch_post_merge_ritual(
        7, dedup_key="shaF", auto_run=True, canonical_root=canonical, spawn=spawn
    )
    assert (canonical / ".fno" / "post-merge-dispatched" / "shaF").exists()
    # the spawn cwd is the canonical, so the worker's ritual resolves canonical too
    assert spawn.calls[0][1] == str(canonical)


# --- task 2.3: merge SHA threading through reconcile ---------------------


def test_query_parses_merge_sha():
    class _Res:
        returncode = 0
        stdout = (
            '{"number": 7, "state": "MERGED", "url": "u", '
            '"mergedAt": "t", "mergeCommit": {"oid": "cafef00d"}}'
        )
        stderr = ""

    def runner(cmd, **kw):
        assert "mergeCommit" in cmd[cmd.index("--json") + 1]
        return _Res()

    state = query_pr_merge_state(7, repo="o/r", runner=runner)
    assert state.merge_sha == "cafef00d"


def test_scan_threads_merge_sha_onto_record():
    entries = [
        {"id": "x-0001", "pr_number": 7, "pr_url": "https://github.com/o/r/pull/7"}
    ]

    def query(number, repo=None, cwd=None):
        return PrMergeState(
            number=number, state="MERGED", url="https://github.com/o/r/pull/7",
            merged_at="t", merge_sha="beefcafe",
        )

    records = scan_merge_drift(entries, query=query, list_merged=lambda **kw: [])
    closeable = [r for r in records if r.closeable]
    assert len(closeable) == 1
    assert closeable[0].merge_sha == "beefcafe"
