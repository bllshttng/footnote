from __future__ import annotations

import json

from fno.pr import _quota
from fno.pr._proc import Result


def _runner(remaining: int | None, calls: list[list[str]], command_result: Result | None = None):
    def run(cmd, **kwargs):
        calls.append(list(cmd))
        if cmd[-2:] == ["api", "rate_limit"]:
            if remaining is None:
                return Result(1, "", "instrument unavailable")
            return Result(
                0,
                json.dumps({"resources": {"graphql": {"remaining": remaining, "reset": 1787072400}}}),
                "",
            )
        return command_result or Result(0, '{"data":{"ok":true}}', "")

    return run


def test_discretionary_read_is_refused_at_floor_before_graphql(tmp_path):
    calls: list[list[str]] = []
    result = _quota.execute_graphql(
        "discretionary",
        ["pr", "view", "930", "--json", "headRefOid"],
        runner=_runner(_quota.GRAPHQL_RESERVE, calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == _quota.REFUSED
    assert calls == [["/real/gh", "api", "rate_limit"]]
    assert "fno pr info 930" in result.stderr
    assert "stop retrying GraphQL until" in result.stderr
    assert "still contains optional review-thread and coverage reads" in result.stderr


def test_coverage_can_consume_reserved_points(tmp_path):
    calls: list[list[str]] = []
    result = _quota.execute_graphql(
        "coverage",
        ["pr", "view", "930", "--json", "reviews"],
        runner=_runner(_quota.GRAPHQL_RESERVE, calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == 0
    assert calls == [
        ["/real/gh", "api", "rate_limit"],
        ["/real/gh", "pr", "view", "930", "--json", "reviews"],
    ]


def test_unreadable_instrument_fails_closed_only_for_discretionary(tmp_path):
    discretionary_calls: list[list[str]] = []
    refused = _quota.execute_graphql(
        "discretionary",
        ["api", "graphql", "-f", "query={viewer{login}}"],
        runner=_runner(None, discretionary_calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert refused.returncode == _quota.REFUSED
    assert "instrument unavailable" in refused.stderr
    assert len(discretionary_calls) == 1

    coverage_calls: list[list[str]] = []
    allowed = _quota.execute_graphql(
        "coverage",
        ["api", "graphql", "-f", "query={viewer{login}}"],
        runner=_runner(None, coverage_calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert allowed.returncode == 0
    assert len(coverage_calls) == 2


def test_only_coverage_spelling_can_claim_the_reserve(tmp_path):
    calls: list[list[str]] = []
    result = _quota.execute_graphql(
        "priority",
        ["api", "graphql", "-f", "query={viewer{login}}"],
        runner=_runner(5000, calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == 2
    assert calls == []
