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

import json
import os
import subprocess

from fno.graph import _reconcile as rec
from fno.graph._reconcile import (
    PrMergeState,
    _ListingCache,
    collect_open_binding_heals,
    scan_merge_drift,
)
from fno.pr import _rest
from fno.pr._proc import Result

REPO_SLUG_URL = "https://github.com/o/r/pull/{n}"
STAMPED = list(range(101, 118))  # 17 PR-carrying open nodes


def _repo_tree(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q", str(repo)], check=True, capture_output=True)
    subprocess.run(
        ["git", "-C", str(repo), "remote", "add", "origin", "https://github.com/o/r.git"],
        check=True,
    )
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
            "state": "closed",
            "merged_at": f"2026-09-0{1 + n % 8}T00:00:00Z",
            "title": f"PR {n}",
            "body": "",
            "head": {"ref": f"feature/stuff-{n}"},
            "html_url": REPO_SLUG_URL.format(n=n),
        }
        for n in numbers
    ]


def _install_fake_rest(monkeypatch, *, merged_rows, open_rows, tmp_path):
    """Record REST listings and answer them from GitHub-shaped fixtures."""
    calls: list[list[str]] = []
    real_run = subprocess.run

    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    fake_gh = bin_dir / "gh"
    fake_gh.write_text("#!/bin/sh\nexit 97\n")
    fake_gh.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ['PATH']}")

    def fake_run(cmd, *, cwd=None, timeout=None, **kwargs):
        calls.append(list(cmd))
        if cmd[:3] == ["git", "remote", "get-url", "origin"]:
            result = real_run(cmd, cwd=cwd, capture_output=True, text=True, check=False)
            return Result(result.returncode, result.stdout, result.stderr)
        assert cmd[:2] == ["gh", "api"], f"unexpected command: {cmd}"
        rows = merged_rows if "state=closed" in cmd[2] else open_rows
        return Result(0, json.dumps(rows), "")

    monkeypatch.setattr(_rest, "run", fake_run)
    monkeypatch.setattr(rec, "_gh_executable", lambda: "/usr/bin/gh")
    return calls, rec.list_open_pr_branches, rec.list_merged_pr_branches


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
            "state": "open",
            "title": "PR 900",
            "body": "",
            "head": {"ref": "feature/ab-aaa0"},
            "html_url": REPO_SLUG_URL.format(n=900),
        }
    ]
    calls, open_seam, merged_seam = _install_fake_rest(
        monkeypatch, merged_rows=_merged_rows(STAMPED), open_rows=open_rows, tmp_path=tmp_path
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
    assert all(c[:2] == ["gh", "api"] for c in gh)
    assert any("state=open" in c[2] for c in gh)
    assert any("state=closed" in c[2] for c in gh)
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
    calls, open_seam, merged_seam = _install_fake_rest(
        monkeypatch, merged_rows=_merged_rows(STAMPED[1:]), open_rows=[], tmp_path=tmp_path
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
    calls, open_seam, merged_seam = _install_fake_rest(
        monkeypatch, merged_rows=_merged_rows(STAMPED), open_rows=[], tmp_path=tmp_path
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
