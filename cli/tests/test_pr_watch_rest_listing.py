"""REST listing + GraphQL-budget reads behind the pr-watch sweep (x-c12c).

The bulk sweep moved off `gh pr list` (GraphQL, point-cost billed, shared
per-user) onto `gh api repos/<slug>/pulls` (REST, measured free against the
untouched core bucket). AC1 is asserted as a POSITIVE match on the REST argv
shape plus an explicit absence of the old `["pr", "list"]` pair, never
absence alone. Fakes speak the `fno.pr._proc.run` contract (Result objects),
the single runner convention the sweep now carries end to end.
"""
from __future__ import annotations

import json
from datetime import datetime, timezone

from fno.pr._proc import Result


def _ok(stdout: str) -> Result:
    return Result(returncode=0, stdout=stdout, stderr="")


def _page(rows: list[dict]) -> Result:
    return _ok(json.dumps(rows))


def _pr_row(number: int, state: str = "open", merged: bool = False) -> dict:
    return {"number": number, "state": state, "merged": merged}


def _has_pr_list_pair(cmd: list) -> bool:
    return any(cmd[i : i + 2] == ["pr", "list"] for i in range(len(cmd) - 1))


# ---------------------------------------------------------------------------
# list_prs_rest
# ---------------------------------------------------------------------------


class TestListPrsRest:
    def test_issues_rest_argv_not_gh_pr_list(self):
        """AC1-HP: the listing is a gh api repos/<slug>/pulls read."""
        from fno.pr._rest import list_prs_rest

        calls: list[list] = []

        def runner(cmd, **_kw):
            calls.append(list(cmd))
            return _page([_pr_row(1)])

        rows, reason = list_prs_rest("owner/repo", runner=runner)

        assert reason == ""
        assert rows == [{"number": 1, "state": "OPEN"}]
        assert calls[0][:3] == [
            "gh", "api", "repos/owner/repo/pulls?state=open&per_page=100&page=1",
        ]
        assert not _has_pr_list_pair(calls[0])

    def test_paginates_until_short_page(self):
        """AC2-HP: a full page 1 continues to page 2; a short page stops."""
        from fno.pr._rest import list_prs_rest

        calls: list[list] = []

        def runner(cmd, **_kw):
            calls.append(list(cmd))
            # Anchored: a bare "page=1" also matches inside "per_page=100".
            if "&page=1" in cmd[2]:
                return _page([_pr_row(n) for n in range(100)])
            return _page([_pr_row(101), _pr_row(102, "closed", merged=True)])

        rows, reason = list_prs_rest("owner/repo", runner=runner)

        assert reason == ""
        assert len(calls) == 2
        assert calls[1][2].endswith("page=2")
        numbers = [r["number"] for r in rows]
        assert 1 in numbers and 102 in numbers and len(rows) == 102
        assert next(r for r in rows if r["number"] == 102)["state"] == "MERGED"

    def test_single_short_page_stops_immediately(self):
        from fno.pr._rest import list_prs_rest

        calls: list[list] = []

        def runner(cmd, **_kw):
            calls.append(list(cmd))
            return _page([_pr_row(7)])

        rows, _ = list_prs_rest("owner/repo", runner=runner)
        assert len(calls) == 1
        assert rows == [{"number": 7, "state": "OPEN"}]

    def test_max_pages_ceiling_bounds_the_loop(self):
        """A pathological repo (always-full pages) stops at the named ceiling."""
        from fno.pr._rest import list_prs_rest

        calls: list[list] = []

        def runner(cmd, **_kw):
            calls.append(list(cmd))
            return _page([_pr_row(n) for n in range(100)])

        rows, _ = list_prs_rest("owner/repo", runner=runner, max_pages=3)
        assert len(calls) == 3
        assert len(rows) == 300

    def test_failure_is_loud_none_plus_reason(self):
        from fno.pr._rest import list_prs_rest

        def runner(cmd, **_kw):
            return Result(returncode=1, stdout="", stderr="gh: API rate limit exceeded")

        rows, reason = list_prs_rest("owner/repo", runner=runner)
        assert rows is None
        assert "rate limit" in reason

    def test_non_json_is_loud(self):
        from fno.pr._rest import list_prs_rest

        def runner(cmd, **_kw):
            return _ok("not json")

        rows, reason = list_prs_rest("owner/repo", runner=runner)
        assert rows is None
        assert "not JSON" in reason


# ---------------------------------------------------------------------------
# graphql_remaining
# ---------------------------------------------------------------------------


class TestGraphqlRemaining:
    def _payload(self, remaining: int, reset_epoch: int) -> str:
        return json.dumps(
            {"resources": {"graphql": {"remaining": remaining, "reset": reset_epoch},
                           "core": {"remaining": 5000, "reset": reset_epoch}}}
        )

    def test_reads_remaining_and_reset(self):
        from fno.pr._rest import graphql_remaining

        reset_epoch = 1755427200
        remaining, reset_iso = graphql_remaining(
            runner=lambda cmd, **_kw: _ok(self._payload(37, reset_epoch))
        )
        assert remaining == 37
        expected = datetime.fromtimestamp(reset_epoch, tz=timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
        assert reset_iso == expected

    def test_failure_reads_as_unknown_never_zero(self):
        """AC6-EDGE backdrop: an unreadable instrument returns None, not 0."""
        from fno.pr._rest import graphql_remaining

        remaining, reset_iso = graphql_remaining(
            runner=lambda cmd, **_kw: Result(returncode=1, stdout="", stderr="gh: broken")
        )
        assert remaining is None and reset_iso is None

    def test_malformed_payload_reads_as_unknown(self):
        from fno.pr._rest import graphql_remaining

        remaining, reset_iso = graphql_remaining(
            runner=lambda cmd, **_kw: _ok('{"resources": {}}')
        )
        assert remaining is None and reset_iso is None
