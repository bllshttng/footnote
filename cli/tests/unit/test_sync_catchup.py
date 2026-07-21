"""Catch-up sweep + staleness alarm for the post-merge canonical sync (x-8a26).

The outage this covers was "the daemon runs but does nothing" for five straight
merges, so every assertion here is on ground truth (marker files on disk, call
counts) rather than on log prose - a passing log line is exactly what lied last
time.
"""
from __future__ import annotations

from datetime import datetime, timedelta, timezone
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.pr import _sync_canonical as sc


def _pm(**over):
    base = dict(
        sync_command="true",
        sync_paths=[],
        auto_run=True,
        catchup_window_days=3,
        sync_stale_hours=24,
    )
    base.update(over)
    return SimpleNamespace(post_merge=SimpleNamespace(**base))


def _merged(number: int, sha: str, hours_ago: float) -> dict:
    return {
        "number": number,
        "sha": sha,
        "merged_at": datetime.now(timezone.utc) - timedelta(hours=hours_ago),
    }


def _gh(rows):
    return lambda _canonical, _window: rows


def _git(behind: int | None):
    """A runner standing in for git: symbolic-ref then rev-list."""

    def runner(cmd, **_kw):
        if behind is None:
            return sc.Result(returncode=1, stdout="", stderr="no upstream")
        if cmd[:2] == ["git", "symbolic-ref"]:
            return sc.Result(returncode=0, stdout="origin/main\n", stderr="")
        return sc.Result(returncode=0, stdout=f"{behind}\n", stderr="")

    return runner


def _marker(root: Path, sha: str) -> Path:
    return root / ".fno" / "post-merge-synced" / sha


def _stamp(root: Path, sha: str) -> None:
    m = _marker(root, sha)
    m.parent.mkdir(parents=True, exist_ok=True)
    m.touch()


# --- sync_staleness ---------------------------------------------------------


def test_staleness_fresh_when_newest_merge_is_marked(tmp_path):
    rows = [_merged(50, "aaa", 48), _merged(49, "bbb", 72)]
    _stamp(tmp_path, "aaa")
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0), gh_list=_gh(rows)
    )
    # bbb is markerless but sits BEHIND a synced head: cosmetic, not an outage.
    assert st.state == "fresh"
    assert [r["sha"] for r in st.markerless] == ["bbb"]


def test_staleness_stale_when_newest_merge_is_old_and_unmarked(tmp_path):
    rows = [_merged(50, "aaa", 48)]
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0), gh_list=_gh(rows)
    )
    assert st.state == "stale"
    assert "#50" in st.detail  # AC2: the offending PR is named


def test_staleness_fresh_when_newest_merge_is_recent(tmp_path):
    """A merge from two minutes ago is not an outage - the tick has not run yet."""
    st = sc.sync_staleness(
        settings=_pm(),
        canonical_root=tmp_path,
        runner=_git(0),
        gh_list=_gh([_merged(50, "aaa", 0.03)]),
    )
    assert st.state == "fresh"
    assert st.markerless  # still swept eagerly, just not alarmed on


def test_staleness_stale_when_behind_origin(tmp_path):
    _stamp(tmp_path, "aaa")
    st = sc.sync_staleness(
        settings=_pm(),
        canonical_root=tmp_path,
        runner=_git(7),
        gh_list=_gh([_merged(50, "aaa", 1)]),
    )
    assert st.state == "stale"
    assert "7 behind" in st.detail


def test_staleness_unknown_when_gh_unavailable(tmp_path):  # AC3-ERR
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0), gh_list=_gh(None)
    )
    assert st.state == "unknown"
    assert st.markerless == ()


def test_staleness_fresh_on_zero_merges(tmp_path):
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0), gh_list=_gh([])
    )
    assert st.state == "fresh"


def test_staleness_never_fetches(tmp_path):
    """A predicate that mutates the repo is not a predicate."""
    seen: list[list[str]] = []

    def runner(cmd, **_kw):
        seen.append(list(cmd))
        return sc.Result(returncode=0, stdout="origin/main\n0\n", stderr="")

    sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=runner, gh_list=_gh([])
    )
    assert not any("fetch" in c for cmd in seen for c in cmd)


def test_gh_list_filters_by_window(tmp_path, monkeypatch):
    import json

    payload = json.dumps([
        {"number": 9, "mergedAt": "2020-01-01T00:00:00Z", "mergeCommit": {"oid": "old"}},
        {"number": 10, "mergedAt": None, "mergeCommit": {"oid": "nodate"}},
        {"number": 11, "mergedAt": _iso(1), "mergeCommit": {"oid": "new"}},
        {"number": 12, "mergedAt": _iso(2), "mergeCommit": None},
    ])
    monkeypatch.setattr(
        sc, "_run", lambda *_a, **_k: sc.Result(returncode=0, stdout=payload, stderr="")
    )
    rows = sc._default_gh_list(tmp_path, 3)
    assert [r["sha"] for r in rows] == ["new"]


def _iso(hours_ago: float) -> str:
    return (
        datetime.now(timezone.utc) - timedelta(hours=hours_ago)
    ).strftime("%Y-%m-%dT%H:%M:%SZ")


# --- run_sync_catchup -------------------------------------------------------


def test_catchup_syncs_newest_and_stamps_the_rest(tmp_path):  # AC1-HP
    rows = [_merged(52, "ccc", 30), _merged(51, "bbb", 40), _merged(50, "aaa", 50)]
    calls: list[int] = []

    def sync(pr, **_kw):
        calls.append(pr)
        _stamp(tmp_path, "ccc")
        return 0

    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh(rows), sync=sync,
    )
    assert calls == [52]  # newest only, one pull covers the rest
    assert res.outcome == "synced" and res.swept == 2
    for sha in ("aaa", "bbb", "ccc"):
        assert _marker(tmp_path, sha).exists()


def test_catchup_failure_stamps_nothing_and_retries(tmp_path):  # AC4-ERR
    rows = [_merged(52, "ccc", 30), _merged(51, "bbb", 40)]
    calls: list[int] = []

    def sync(pr, **_kw):
        calls.append(pr)
        return 1

    kw = dict(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh(rows), sync=sync,
    )
    assert sc.run_sync_catchup(**kw).outcome == "failed"
    assert not _marker(tmp_path, "ccc").exists()
    assert not _marker(tmp_path, "bbb").exists()

    sc.run_sync_catchup(**kw)  # a later firing retries the same merge
    assert calls == [52, 52]


def test_catchup_declined_sync_stamps_nothing(tmp_path):  # AC7-EDGE
    """A claim-held loser exits 0 without syncing; it must not backdate markers."""
    rows = [_merged(52, "ccc", 30), _merged(51, "bbb", 40)]
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh(rows), sync=lambda pr, **_kw: 0,  # exits 0, writes no marker
    )
    assert res.outcome == "skipped"
    assert not _marker(tmp_path, "bbb").exists()


def test_catchup_inert_when_auto_run_off(tmp_path):  # AC6-EDGE
    calls: list[int] = []
    res = sc.run_sync_catchup(
        settings=_pm(auto_run=False), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh([_merged(52, "ccc", 30)]),
        sync=lambda pr, **_kw: calls.append(pr) or 0,
    )
    assert res.outcome == "disabled"
    assert calls == []


def test_catchup_skips_on_gh_failure(tmp_path, capsys):  # AC3-ERR
    calls: list[int] = []
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh(None), sync=lambda pr, **_kw: calls.append(pr) or 0,
    )
    assert res.outcome == "unknown"
    assert calls == []
    assert len(capsys.readouterr().err.strip().splitlines()) == 1  # exactly one warning


def test_catchup_fresh_when_all_marked(tmp_path):
    _stamp(tmp_path, "ccc")
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh([_merged(52, "ccc", 30)]),
        sync=lambda pr, **_kw: pytest.fail("must not sync a current canonical"),
    )
    assert res.outcome == "fresh"
