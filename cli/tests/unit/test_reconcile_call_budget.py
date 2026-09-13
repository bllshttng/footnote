"""The reconcile scans pay one gh listing per repo, not per worktree or node.

Pure-module tests over a fixture graph of six same-repo worktree cwds and 17
PR-carrying open nodes, with a counting runner bound into the real listing
functions so every gh command is recorded with its real argv. The assertions
are positive markers, not absences: exactly one ``--state open`` and one
``--state merged`` listing must appear in the recorded commands, and the
per-node fallback query must fire zero times (or exactly once, for the one
number absent from both listings). No wall clock is asserted; the call count
is the contract. The scans run in production order: the open-binding heal
first, warming the shared cache the drift scan reads.
"""
from __future__ import annotations

import functools
import json
import subprocess

from fno.graph import _reconcile as rec
from fno.graph._reconcile import (
    PrMergeState,
    _ListingCache,
    collect_open_binding_heals,
    scan_merge_drift,
)

REPO_SLUG_URL = "https://github.com/o/r/pull/{n}"
STAMPED = list(range(101, 118))  # 17 PR-carrying open nodes


def _repo_tree(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q", str(repo)], check=True, capture_output=True)
    dirs = []
    for i in range(6):
        d = repo / f"wt-{i}"
        d.mkdir()
        dirs.append(d)
    return dirs


def _entries(dirs):
    out = [
        {"id": f"ab-aaa{i}", "status": "ready", "cwd": str(d)}
        for i, d in enumerate(dirs)
    ]
    for i, n in enumerate(STAMPED):
        out.append({
            "id": f"ab-bbb{i:02d}", "status": "in_progress",
            "cwd": str(dirs[i % 6]),
            "pr_number": n, "pr_url": REPO_SLUG_URL.format(n=n),
        })
    return out


def _merged_rows(numbers):
    return [
        {
            "number": n,
            "url": REPO_SLUG_URL.format(n=n),
            "headRefName": f"feature/stuff-{n}",
            "mergedAt": f"2026-09-0{1 + n % 8}T00:00:00Z",
        }
        for n in numbers
    ]


def _install_fake_gh(monkeypatch, *, merged_rows, open_rows):
    """Record every command with its argv; answer gh pr list from fixtures."""
    real_run = subprocess.run
    calls: list[list[str]] = []

    def fake_run(cmd, **kwargs):
        calls.append(list(cmd))
        if cmd[0] == "git":
            return real_run(cmd, **kwargs)
        assert cmd[:3] == ["gh", "pr", "list"], f"unexpected gh command: {cmd}"
        rows = merged_rows if "merged" in cmd else open_rows
        return subprocess.CompletedProcess(cmd, 0, stdout=json.dumps(rows), stderr="")

    monkeypatch.setattr(rec.subprocess, "run", fake_run)
    monkeypatch.setattr(rec, "_gh_executable", lambda: "/usr/bin/gh")
    return calls, functools.partial(rec.list_open_pr_branches, runner=fake_run), \
        functools.partial(rec.list_merged_pr_branches, runner=fake_run)


def _gh_calls(calls):
    return [c for c in calls if c[0] == "gh"]


def _closed_query(counter):
    def query(number, repo=None, cwd=None):
        counter.append(number)
        return PrMergeState(number=number, state="CLOSED", url=None, merged_at=None)

    return query


def test_two_listings_answer_the_whole_scan_pair(tmp_path, monkeypatch):
    dirs = _repo_tree(tmp_path)
    entries = _entries(dirs)
    open_rows = [
        {
            "number": 900,
            "url": REPO_SLUG_URL.format(n=900),
            "headRefName": "feature/ab-aaa0",
            "body": "",
        }
    ]
    calls, open_seam, merged_seam = _install_fake_gh(
        monkeypatch, merged_rows=_merged_rows(STAMPED), open_rows=open_rows
    )

    listings = _ListingCache()
    heals, advisories = collect_open_binding_heals(
        entries, list_open=open_seam, listings=listings
    )
    counter: list[int] = []
    records = scan_merge_drift(
        entries, list_merged=merged_seam, listings=listings,
        query=_closed_query(counter),
    )

    gh = _gh_calls(calls)
    assert len(gh) == 2, gh
    assert any("--state" in c and "open" in c for c in gh)
    assert any("--state" in c and "merged" in c for c in gh)
    assert not any("view" in c for c in gh)
    assert counter == [], "every stamped number resolved from a listing"
    assert len(heals) == 1 and heals[0].pr_number == 900
    assert advisories == []

    # The listing answer equals the per-node-query answer for the same fixture.
    merged_at = {n: f"2026-09-0{1 + n % 8}T00:00:00Z" for n in STAMPED}

    def _merged_query(number, repo=None, cwd=None):
        return PrMergeState(
            number=number, state="MERGED",
            url=REPO_SLUG_URL.format(n=number), merged_at=merged_at[number],
        )

    per_node = scan_merge_drift(
        entries, query=_merged_query, list_merged=lambda **kw: []
    )
    assert records == per_node


def test_number_in_neither_listing_fires_the_query_exactly_once(tmp_path, monkeypatch):
    dirs = _repo_tree(tmp_path)
    entries = _entries(dirs)
    stray = next(e for e in entries if e.get("pr_number") == STAMPED[0])
    stray["pr_number"] = 999
    stray["pr_url"] = REPO_SLUG_URL.format(n=999)
    calls, open_seam, merged_seam = _install_fake_gh(
        monkeypatch, merged_rows=_merged_rows(STAMPED[1:]), open_rows=[]
    )

    listings = _ListingCache()
    collect_open_binding_heals(entries, list_open=open_seam, listings=listings)
    counter: list[int] = []
    records = scan_merge_drift(
        entries, list_merged=merged_seam, listings=listings,
        query=_closed_query(counter),
    )

    assert counter == [999]
    assert len([r for r in records if r.closeable]) == len(STAMPED) - 1
    assert not [r for r in records if r.node_id == stray["id"]]


def test_pending_supersession_successor_takes_the_query_not_the_listing(
    tmp_path, monkeypatch
):
    """A supersession awaiting the successor's changed-file evidence can never
    verify off a listing row (it carries none): that node must take the
    per-node query, whose default reader fetches the changed files."""
    dirs = _repo_tree(tmp_path)
    entries = _entries(dirs)
    successor = next(e for e in entries if e.get("pr_number") == STAMPED[0])
    entries.append({
        "id": "ab-pred", "status": "ready", "cwd": str(dirs[0]),
        "superseded_by": successor["id"],
        "supersession": {"cause": "old bug", "surfaces": ["src/a.py"]},
    })
    calls, open_seam, merged_seam = _install_fake_gh(
        monkeypatch, merged_rows=_merged_rows(STAMPED), open_rows=[]
    )

    listings = _ListingCache()
    collect_open_binding_heals(entries, list_open=open_seam, listings=listings)
    counter: list[int] = []
    scan_merge_drift(
        entries, list_merged=merged_seam, listings=listings,
        query=_closed_query(counter),
    )

    assert counter == [successor["pr_number"]], "successor resolved by query, not listing"
    assert _gh_calls(calls), "listing fetches still happened for the rest"


def test_cwd_outside_any_repo_groups_under_itself(tmp_path):
    plain = [tmp_path / "not-a-repo-a", tmp_path / "not-a-repo-b"]
    for d in plain:
        d.mkdir()
    entries = [
        {"id": "ab-x1", "status": "ready", "cwd": str(plain[0])},
        {"id": "ab-x2", "status": "ready", "cwd": str(plain[1])},
    ]
    asked: list[str] = []

    def list_open(*, cwd):
        asked.append(cwd)
        return []

    heals, advisories = collect_open_binding_heals(entries, list_open=list_open)
    assert asked == [str(d) for d in plain]
    assert heals == [] and advisories == []


def test_gh_absent_is_quiet(tmp_path, monkeypatch):
    dirs = _repo_tree(tmp_path)
    entries = _entries(dirs)
    real_run = subprocess.run
    calls: list[list[str]] = []

    def fake_run(cmd, **kwargs):
        calls.append(list(cmd))
        return real_run(cmd, **kwargs)

    monkeypatch.setattr(rec.subprocess, "run", fake_run)
    monkeypatch.setattr(rec, "_gh_executable", lambda: None)

    listings = _ListingCache()
    heals, advisories = collect_open_binding_heals(entries, listings=listings)
    records = scan_merge_drift(entries, listings=listings, query=_closed_query([]))

    assert _gh_calls(calls) == []
    assert heals == [] and advisories == []
    assert records == []
