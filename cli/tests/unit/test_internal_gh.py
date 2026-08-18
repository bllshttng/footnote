from __future__ import annotations

import json

from fno.pr import _internal_gh
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


def test_review_read_uses_fixed_purpose_broker(monkeypatch):
    seen = {}

    def execute_graphql(purpose, args, **kwargs):
        seen["purpose"] = purpose
        seen["args"] = list(args)
        return Result(0, '{"reviews":[]}', "")

    monkeypatch.setattr(_internal_gh._quota, "execute_graphql", execute_graphql)
    result = _internal_gh.execute(
        "coverage",
        ["pr", "view", "930", "--json", "reviews,comments"],
        real_gh="/real/gh",
    )
    assert result.returncode == 0
    assert seen == {
        "purpose": "coverage",
        "args": ["pr", "view", "930", "--json", "reviews,comments"],
    }


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
