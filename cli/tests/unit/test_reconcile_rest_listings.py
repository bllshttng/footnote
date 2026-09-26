from __future__ import annotations

import json
import os
import subprocess

import pytest

from fno.graph import _reconcile as rec
from fno.pr import _rest
from fno.pr._proc import Result


def _pr(number, *, state="closed", merged_at=None):
    return {
        "number": number,
        "state": state,
        "merged_at": merged_at,
        "title": f"PR {number}",
        "body": "",
        "head": {"ref": f"feature/x-{number:04x}"},
        "html_url": f"https://github.com/o/r/pull/{number}",
    }


def _checkout(tmp_path):
    cwd = tmp_path / "repo"
    cwd.mkdir()
    subprocess.run(["git", "init", "-q", str(cwd)], check=True)
    subprocess.run(
        ["git", "-C", str(cwd), "remote", "add", "origin", "https://github.com/o/r.git"],
        check=True,
    )
    return cwd


def _install(monkeypatch, payloads, *, tmp_path):
    calls = []
    real_run = subprocess.run

    def fail_legacy_gh(directory):
        bin_dir = directory / "bin"
        bin_dir.mkdir(exist_ok=True)
        gh = bin_dir / "gh"
        gh.write_text("#!/bin/sh\nexit 97\n")
        gh.chmod(0o755)
        monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ['PATH']}")

    fail_legacy_gh(tmp_path)

    def run(cmd, *, cwd=None, timeout=None, **kwargs):
        calls.append(list(cmd))
        if cmd[:3] == ["git", "remote", "get-url"]:
            result = real_run(cmd, cwd=cwd, capture_output=True, text=True, check=False)
            return Result(result.returncode, result.stdout, result.stderr)
        page = int(cmd[2].rsplit("page=", 1)[1])
        rows = payloads[page - 1]
        if rows is None:
            return Result(1, "", "fno config: ignored\nHTTP 502: Bad Gateway")
        return Result(0, json.dumps(rows), "")

    monkeypatch.setattr(_rest, "run", run)
    monkeypatch.setattr(rec, "_gh_executable", lambda: "/usr/bin/gh")
    return calls


def test_merged_listing_uses_rest_and_keeps_only_merged_rows(tmp_path, monkeypatch):
    cwd = _checkout(tmp_path)
    merged = _pr(5, merged_at="2026-09-23T00:00:00Z")
    unmerged = _pr(6)
    calls = _install(monkeypatch, [[merged, unmerged]], tmp_path=tmp_path)

    rows = rec.list_merged_pr_branches(cwd=str(cwd))

    assert rows == [
        {
            "number": 5,
            "state": "MERGED",
            "title": "PR 5",
            "body": "",
            "headRefName": "feature/x-0005",
            "url": "https://github.com/o/r/pull/5",
            "mergedAt": "2026-09-23T00:00:00Z",
        }
    ]
    assert calls[-1] == [
        "gh", "api", "repos/o/r/pulls?state=closed&per_page=100&page=1"
    ]
    assert not any(cmd[:2] == ["gh", "pr"] for cmd in calls)


def test_rest_listing_error_drops_config_warning(tmp_path, monkeypatch):
    cwd = _checkout(tmp_path)
    calls = _install(monkeypatch, [None], tmp_path=tmp_path)

    with pytest.raises(rec.ReconcileError) as exc:
        rec.list_merged_pr_branches(cwd=str(cwd))

    assert "HTTP 502" in str(exc.value)
    assert "fno config" not in str(exc.value)
    assert calls[-1][0:2] == ["gh", "api"]


def test_recent_merged_listing_uses_rest_and_preserves_limit(tmp_path, monkeypatch):
    cwd = _checkout(tmp_path)
    merged = _pr(5, merged_at="2026-09-23T00:00:00Z")
    calls = _install(monkeypatch, [[merged]], tmp_path=tmp_path)

    rows = rec.fetch_recent_merged_prs(cwd=str(cwd), limit=1)

    assert rows[0]["number"] == 5
    assert rows[0]["mergedAt"] == "2026-09-23T00:00:00Z"
    assert calls[-1][2] == "repos/o/r/pulls?state=closed&per_page=100&page=1"
