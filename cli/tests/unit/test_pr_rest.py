"""Tests for the REST settledness reader behind `fno pr status`.

The load-bearing guard: the settledness reader issues NO GraphQL call, so a
watching fleet spends the idle core budget instead of the shared per-USER
GraphQL quota. Every other test pins the mapping (REST payloads -> the rollup
shape `verdict_for` already classifies) and the loud-failure contract: a failed
read is `(None, reason)`, which `run_status` must render as
`verdict: error, settled: false` - never as an absent answer.
"""
from __future__ import annotations

import json

from fno.pr import _rest, _status
from fno.pr._proc import Result

_PULLS = {"state": "open", "merged": False, "head": {"sha": "abc123def"}}
_GH_URL = "git@github.com:Owner/Repo.git"


def _runner(*, pulls=_PULLS, check_runs=(), statuses=None, fail=None, calls=None):
    """Dispatch by URL shape: git remote -> slug, gh api -> REST payloads."""

    def r(cmd, cwd=None):
        if calls is not None:
            calls.append(list(cmd))
        url = cmd[-1] if len(cmd) > 1 else ""
        if fail is not None and fail(cmd):
            return Result(1, "", fail(cmd))
        if cmd[:2] == ["git", "remote"]:
            return Result(0, _GH_URL, "")
        if "/pulls/" in url:
            return Result(0, json.dumps(pulls), "")
        if "check-runs" in url:
            return Result(0, json.dumps({"check_runs": list(check_runs)}), "")
        if url.endswith("/status"):
            return Result(0, json.dumps({"statuses": list(statuses or [])}), "")
        return Result(1, "", "unexpected: " + " ".join(cmd))

    return r


def _cr(name, status, conclusion="", started="2026-08-14T10:00:00Z"):
    return {"name": name, "status": status, "conclusion": conclusion, "started_at": started}


def test_settledness_reader_issues_no_graphql_call():
    """The node's named guard: no `gh pr view` / `gh api graphql` argv, ever."""
    calls: list[list[str]] = []
    r = _runner(
        check_runs=[_cr("ci", "completed", "success")],
        statuses=[{"context": "legacy", "state": "SUCCESS", "created_at": "2026-08-14T10:00:00Z"}],
        calls=calls,
    )
    pr_json, reason = _rest.fetch_pr_rest("42", runner=r)
    assert reason == "" and pr_json is not None
    graphql = [
        c for c in calls
        if c[:3] == ["gh", "pr", "view"] or (len(c) > 2 and c[2] == "graphql")
    ]
    assert graphql == [], f"settledness read spent GraphQL: {graphql}"
    # The reads that DID fire are the three REST endpoints + the local slug.
    assert any("/pulls/42" in c[-1] for c in calls)
    assert any("check-runs" in c[-1] for c in calls)
    assert any(c[-1].endswith("/status") for c in calls)
    assert any(c[:2] == ["git", "remote"] for c in calls)


def test_pr_info_uses_one_rest_request_and_returns_positive_metadata():
    calls: list[list[str]] = []
    pulls = {
        "state": "open",
        "merged": False,
        "mergeable": True,
        "head": {"sha": "abc123def", "ref": "feature/rest-info"},
        "base": {"ref": "main"},
    }
    info, reason = _rest.fetch_pr_info_rest(
        "42", repo="Owner/Repo", runner=_runner(pulls=pulls, calls=calls)
    )
    assert reason == ""
    assert info == {
        "pr": 42,
        "state": "OPEN",
        "head_sha": "abc123def",
        "head_ref": "feature/rest-info",
        "base_ref": "main",
        "mergeable": "MERGEABLE",
    }
    assert calls == [["gh", "api", "repos/Owner/Repo/pulls/42"]]


def test_pr_info_preserves_unknown_mergeability():
    pulls = {
        "state": "open",
        "merged": False,
        "mergeable": None,
        "head": {"sha": "abc123def", "ref": "feature/rest-info"},
        "base": {"ref": "main"},
    }
    info, reason = _rest.fetch_pr_info_rest(
        "42", repo="Owner/Repo", runner=_runner(pulls=pulls)
    )
    assert reason == ""
    assert info["mergeable"] == "UNKNOWN"


def test_current_pr_number_uses_rest_not_gh_pr_view():
    calls: list[list[str]] = []

    def runner(cmd, cwd=None):
        calls.append(list(cmd))
        if cmd[:3] == ["git", "branch", "--show-current"]:
            return Result(0, "feature/rest-info\n", "")
        if cmd[:2] == ["gh", "api"]:
            return Result(0, '[{"number":930}]', "")
        return Result(1, "", "unexpected")

    number, reason = _rest.resolve_current_pr_number_rest(
        repo="Owner/Repo", runner=runner
    )
    assert (number, reason) == (930, "")
    assert calls == [
        ["git", "branch", "--show-current"],
        [
            "gh", "api",
            "repos/Owner/Repo/pulls?state=all&head=Owner:feature/rest-info&per_page=2",
        ],
    ]


def test_rest_green_maps_to_rollup_green():
    r = _runner(
        check_runs=[
            _cr("ci", "completed", "success", "2026-08-14T10:00:00Z"),
            _cr("lint", "completed", "success", "2026-08-14T10:01:00Z"),
        ],
    )
    pr_json, reason = _rest.fetch_pr_rest("42", runner=r)
    assert reason == ""
    verdict, code, counts = _status.verdict_for(pr_json["statusCheckRollup"])
    assert (verdict, code) == ("green", 0)
    assert counts["total"] == 2
    assert pr_json["headRefOid"] == "abc123def"
    assert pr_json["state"] == "OPEN"


def test_rest_in_progress_is_pending_not_red():
    r = _runner(check_runs=[_cr("ci", "in_progress", "")])
    pr_json, _ = _rest.fetch_pr_rest("42", runner=r)
    verdict, code, _ = _status.verdict_for(pr_json["statusCheckRollup"])
    assert (verdict, code) == ("pending", 2)


def test_rest_superseded_run_dedup_survives_the_port():
    """REST returns superseded runs in the same list (a known dedup hazard):
    the port must classify only the latest attempt per name."""
    r = _runner(
        check_runs=[
            _cr("ci", "completed", "cancelled", "2026-08-14T09:55:00Z"),
            _cr("ci", "completed", "success", "2026-08-14T10:00:00Z"),
        ],
    )
    pr_json, _ = _rest.fetch_pr_rest("42", runner=r)
    verdict, code, counts = _status.verdict_for(pr_json["statusCheckRollup"])
    assert (verdict, code) == ("green", 0)
    assert counts["total"] == 1


def test_rest_failure_is_loud_with_transport_named():
    """A failed REST read must NOT read as settled: (None, reason) reaches the
    caller as `verdict: error, settled: false` with the failure class named."""

    def fail(cmd):
        if "check-runs" in cmd[-1]:
            return "HTTP 403: You have exceeded a secondary rate limit"
        return None

    r = _runner(fail=fail)
    pr_json, reason = _rest.fetch_pr_rest("42", runner=r)
    assert pr_json is None
    assert "secondary rate limit" in reason.lower()


def test_rest_secondary_limit_reason_tells_the_caller_to_back_off():
    res = Result(1, "", "HTTP 403: You have exceeded a secondary rate limit")
    reason = _rest._rest_reason(res)
    assert "SECONDARY" in reason
    assert "back off" in reason.lower()


def test_rest_primary_limit_reason_names_core_bucket():
    res = Result(1, "", "HTTP 403: API rate limit exceeded for 12:00:00Z")
    reason = _rest._rest_reason(res)
    assert "CORE" in reason
    assert "resources.core" in reason


def test_rest_merged_state_maps():
    r = _runner(pulls={"state": "closed", "merged": True, "head": {"sha": "s"}})
    pr_json, _ = _rest.fetch_pr_rest("42", runner=r)
    assert pr_json["state"] == "MERGED"


def test_rest_non_numeric_pr_is_a_loud_error():
    pr_json, reason = _rest.fetch_pr_rest("feature/x", runner=_runner())
    assert pr_json is None
    assert "numeric" in reason


def test_rest_unresolvable_slug_is_a_loud_error():
    r = _runner()

    def bad_git(cmd, cwd=None):
        if cmd[:2] == ["git", "remote"]:
            return Result(1, "", "no origin")
        return r(cmd, cwd)

    pr_json, reason = _rest.fetch_pr_rest("42", runner=bad_git)
    assert pr_json is None
    assert "owner/repo" in reason


def test_rest_bad_json_is_a_loud_error():
    def r2(cmd, cwd=None):
        if cmd[:2] == ["git", "remote"]:
            return Result(0, _GH_URL, "")
        if "/pulls/" in cmd[-1]:
            return Result(0, "not json", "")
        return Result(1, "", "?")

    pr_json, reason = _rest.fetch_pr_rest("42", runner=r2)
    assert pr_json is None
    assert "not JSON" in reason


def test_status_context_entries_map_through_classify():
    r = _runner(
        check_runs=[_cr("ci", "completed", "success")],
        statuses=[{"context": "deploy", "state": "pending", "created_at": ""}],
    )
    pr_json, _ = _rest.fetch_pr_rest("42", runner=r)
    verdict, code, counts = _status.verdict_for(pr_json["statusCheckRollup"])
    assert (verdict, code) == ("pending", 2)
    assert counts["total"] == 2
