from __future__ import annotations

import json

import pytest

from fno.pr import _internal_gh, _quota
from fno.pr._proc import Result


PULL = {
    "number": 930,
    "html_url": "https://github.com/o/r/pull/930",
    "state": "open",
    "merged": False,
    "mergeable": True,
    "head": {"sha": "abc123", "ref": "feature/test"},
    "base": {"ref": "main"},
}


def _runner(calls: list[list[str]]):
    def run(cmd, **kwargs):
        calls.append(list(cmd))
        endpoint = cmd[-1]
        if endpoint == "repos/o/r/pulls/930":
            return Result(0, json.dumps(PULL), "")
        if "check-runs" in endpoint:
            return Result(
                0,
                json.dumps(
                    {
                        "total_count": 1,
                        "check_runs": [
                            {
                                "name": "unit",
                                "status": "completed",
                                "conclusion": "success",
                                "started_at": "2026-08-18T00:00:00Z",
                            }
                        ],
                    }
                ),
                "",
            )
        if endpoint.endswith("/status"):
            return Result(0, '{"statuses":[]}', "")
        if cmd[-2:] == ["api", "rate_limit"]:
            return Result(
                0,
                '{"resources":{"graphql":{"remaining":5000,"reset":1787072400}}}',
                "",
            )
        return Result(0, '{"reviews":[]}', "")

    return run


def test_metadata_uses_one_rest_pull_and_no_graphql(monkeypatch):
    calls: list[list[str]] = []
    monkeypatch.setattr(_internal_gh._rest, "_repo_slug", lambda cwd, runner: "o/r")
    result = _internal_gh.execute(
        "discretionary",
        ["pr", "view", "930", "--json", "state,number,headRefOid,mergeable"],
        runner=_runner(calls),
        real_gh="/real/gh",
    )
    assert result.returncode == 0
    assert json.loads(result.stdout)["headRefOid"] == "abc123"
    assert calls == [["/real/gh", "api", "repos/o/r/pulls/930"]]


def test_checks_translate_rest_rollup_to_gh_bucket_shape(monkeypatch):
    calls: list[list[str]] = []
    monkeypatch.setattr(_internal_gh._rest, "_repo_slug", lambda cwd, runner: "o/r")
    result = _internal_gh.execute(
        "discretionary",
        ["pr", "checks", "930", "--json", "name,state,bucket,startedAt,workflow"],
        runner=_runner(calls),
        real_gh="/real/gh",
    )
    assert result.returncode == 0
    assert json.loads(result.stdout) == [
        {
            "name": "unit",
            "state": "success",
            "bucket": "pass",
            "startedAt": "2026-08-18T00:00:00Z",
            "workflow": "",
        }
    ]
    assert all("graphql" not in " ".join(call) for call in calls)


def test_coverage_review_read_composes_rest_evidence(monkeypatch):
    calls: list[list[str]] = []
    clean_body = (
        "Codex Review: Didn’t find any major issues. Bravo. "
        "Reviewed commit: abc123"
    )

    def runner(cmd, **kwargs):
        calls.append(list(cmd))
        endpoint = cmd[-1]
        if endpoint == "repos/o/r/pulls/930":
            return Result(0, json.dumps(PULL), "")
        if endpoint == "repos/o/r/pulls/930/reviews?per_page=100&page=1":
            return Result(0, "[]", "")
        if endpoint == "repos/o/r/issues/930/comments?per_page=100&page=1":
            return Result(
                0,
                json.dumps(
                    [
                        {
                            "user": {"login": "chatgpt-codex-connector[bot]"},
                            "created_at": "2026-08-17T00:00:00Z",
                            "body": clean_body,
                        }
                    ]
                ),
                "",
            )
        raise AssertionError(cmd)

    monkeypatch.setattr(_internal_gh._rest, "_repo_slug", lambda cwd, runner: "o/r")
    monkeypatch.setattr(
        _internal_gh._quota,
        "execute_graphql",
        lambda *a, **k: pytest.fail("coverage review reads must not use GraphQL"),
    )
    result = _internal_gh.execute(
        "coverage",
        [
            "pr",
            "view",
            "930",
            "--json",
            "reviews,comments,headRefOid,baseRefName",
        ],
        runner=runner,
        real_gh="/real/gh",
    )
    assert result.returncode == 0
    payload = json.loads(result.stdout)
    assert payload["headRefOid"] == "abc123"
    assert payload["baseRefName"] == "main"
    assert payload["reviews"] == []
    assert payload["comments"] == [
        {
            "author": {"login": "chatgpt-codex-connector[bot]"},
            "createdAt": "2026-08-17T00:00:00Z",
            "body": clean_body,
        }
    ]
    assert all("graphql" not in " ".join(call) for call in calls)


def test_coverage_review_read_preserves_refusal_body_byte_for_byte(monkeypatch):
    refusal = "You have reached your Codex usage limits for code reviews"

    def runner(cmd, **kwargs):
        endpoint = cmd[-1]
        if endpoint == "repos/o/r/pulls/930":
            return Result(0, json.dumps(PULL), "")
        if "/reviews?" in endpoint:
            return Result(0, "[]", "")
        if "/comments?" in endpoint:
            return Result(
                0,
                json.dumps(
                    [
                        {
                            "user": {"login": "chatgpt-codex-connector[bot]"},
                            "created_at": "2026-08-17T00:00:00Z",
                            "body": refusal,
                        }
                    ]
                ),
                "",
            )
        raise AssertionError(cmd)

    monkeypatch.setattr(_internal_gh._rest, "_repo_slug", lambda cwd, runner: "o/r")
    result = _internal_gh.execute(
        "coverage",
        ["pr", "view", "930", "--json", "reviews,comments"],
        runner=runner,
        real_gh="/real/gh",
    )
    assert result.returncode == 0
    assert json.loads(result.stdout)["comments"][0]["body"] == refusal


def test_coverage_review_read_empty_collections_are_known_empty(monkeypatch):
    def runner(cmd, **kwargs):
        endpoint = cmd[-1]
        if endpoint == "repos/o/r/pulls/930":
            return Result(0, json.dumps(PULL), "")
        if "/reviews?" in endpoint or "/comments?" in endpoint:
            return Result(0, "[]", "")
        raise AssertionError(cmd)

    monkeypatch.setattr(_internal_gh._rest, "_repo_slug", lambda cwd, runner: "o/r")
    result = _internal_gh.execute(
        "coverage",
        ["pr", "view", "930", "--json", "reviews,comments"],
        runner=runner,
        real_gh="/real/gh",
    )
    assert result.returncode == 0
    assert json.loads(result.stdout) == {
        "reviews": [],
        "comments": [],
        "headRefOid": "abc123",
        "baseRefName": "main",
    }


def test_coverage_review_read_paginates_both_collections(monkeypatch):
    def runner(cmd, **kwargs):
        endpoint = cmd[-1]
        if endpoint == "repos/o/r/pulls/930":
            return Result(0, json.dumps(PULL), "")
        if "&page=1" in endpoint:
            kind = "review" if "/reviews?" in endpoint else "comment"
            rows = [{"id": i, "user": {"login": f"{kind}-{i}"}} for i in range(100)]
            return Result(0, json.dumps(rows), "")
        if "&page=2" in endpoint:
            return Result(0, json.dumps([{"id": 100, "user": {"login": "last"}}]), "")
        raise AssertionError(cmd)

    monkeypatch.setattr(_internal_gh._rest, "_repo_slug", lambda cwd, runner: "o/r")
    result = _internal_gh.execute(
        "coverage",
        ["pr", "view", "930", "--json", "reviews,comments"],
        runner=runner,
        real_gh="/real/gh",
    )
    payload = json.loads(result.stdout)
    assert result.returncode == 0
    assert len(payload["reviews"]) == 101
    assert len(payload["comments"]) == 101
    assert payload["reviews"][-1]["author"]["login"] == "last"
    assert payload["comments"][-1]["author"]["login"] == "last"


@pytest.mark.parametrize("failed_collection", ["reviews", "comments"])
def test_coverage_review_read_fails_when_either_collection_fails(
    monkeypatch, failed_collection
):
    def runner(cmd, **kwargs):
        endpoint = cmd[-1]
        if endpoint == "repos/o/r/pulls/930":
            return Result(0, json.dumps(PULL), "")
        if f"/{failed_collection}?" in endpoint:
            return Result(1, "", f"{failed_collection} unavailable")
        return Result(0, "[]", "")

    monkeypatch.setattr(_internal_gh._rest, "_repo_slug", lambda cwd, runner: "o/r")
    result = _internal_gh.execute(
        "coverage",
        ["pr", "view", "930", "--json", "reviews,comments"],
        runner=runner,
        real_gh="/real/gh",
    )
    assert result.returncode == 1
    assert failed_collection in result.stderr


def test_coverage_review_read_fails_on_malformed_collection(monkeypatch):
    def runner(cmd, **kwargs):
        endpoint = cmd[-1]
        if endpoint == "repos/o/r/pulls/930":
            return Result(0, json.dumps(PULL), "")
        if "/reviews?" in endpoint:
            return Result(0, "{}", "")
        return Result(0, "[]", "")

    monkeypatch.setattr(_internal_gh._rest, "_repo_slug", lambda cwd, runner: "o/r")
    result = _internal_gh.execute(
        "coverage",
        ["pr", "view", "930", "--json", "reviews,comments"],
        runner=runner,
        real_gh="/real/gh",
    )
    assert result.returncode == 1
    assert "reviews" in result.stderr
    assert "JSON array" in result.stderr


def test_numeric_option_value_is_not_mistaken_for_pr(monkeypatch):
    monkeypatch.setattr(
        _internal_gh._rest,
        "resolve_current_pr_number_rest",
        lambda **kwargs: (42, ""),
    )
    number, reason = _internal_gh._pr_number(
        ["pr", "view", "--jq", "930", "--json=state"], cwd=None, runner=lambda *a: None
    )
    assert (number, reason) == (42, "")


def test_ambiguous_pr_selectors_are_rejected():
    number, reason = _internal_gh._pr_number(
        ["pr", "view", "42", "43", "--json", "state"], cwd=None, runner=lambda *a: None
    )
    assert number is None
    assert "ambiguous" in reason


def test_execute_turns_a_proxy_identity_failure_into_a_result(monkeypatch):
    def broken():
        raise _quota.ProxyIdentityError("config load failed")

    monkeypatch.setattr(_quota, "resolve_real_gh", broken)
    result = _internal_gh.execute("discretionary", ["auth", "status"])
    assert result.returncode == 2
    assert "cannot identify its own install directory" in result.stderr
