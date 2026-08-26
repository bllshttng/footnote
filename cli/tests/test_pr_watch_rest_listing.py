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
import subprocess
from datetime import datetime, timezone

from fno.pr._proc import Result


def _ok(stdout: str) -> Result:
    return Result(returncode=0, stdout=stdout, stderr="")


def _page(rows: list[dict]) -> Result:
    return _ok(json.dumps(rows))


def _pr_row(
    number: int,
    state: str = "open",
    merged: bool = False,
    merged_at: str | None = None,
) -> dict:
    row = {"number": number, "state": state, "merged": merged}
    if merged_at is not None:
        row["merged_at"] = merged_at
    return row


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

    def test_requested_numbers_stop_closed_listing_after_all_are_found(self):
        """AC3-HP: terminal batches stop once every requested number is present."""
        from fno.pr._rest import list_prs_rest

        calls: list[list] = []

        def runner(cmd, **_kw):
            calls.append(list(cmd))
            return _page([_pr_row(9, "closed", merged=True)])

        rows, reason = list_prs_rest(
            "owner/repo", state="closed", requested_numbers={9}, runner=runner
        )

        assert reason == ""
        assert rows == [{"number": 9, "state": "MERGED"}]
        assert len(calls) == 1
        assert "state=closed" in calls[0][2]

    def test_closed_listing_uses_merged_at_to_map_merged(self):
        """Closed list rows expose merge truth through ``merged_at``."""
        from fno.pr._rest import list_prs_rest

        rows, reason = list_prs_rest(
            "owner/repo",
            state="closed",
            runner=lambda *_args, **_kwargs: _page(
                [_pr_row(9, "closed", merged_at="2026-08-24T18:00:00Z")]
            ),
        )

        assert reason == ""
        assert rows == [{"number": 9, "state": "MERGED"}]

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


# ---------------------------------------------------------------------------
# read_tracked_pr_states on REST (the sweep itself)
# ---------------------------------------------------------------------------


def _sweep_runner(calls, *, open_pages, per_key):
    """Answer the sweep's two read shapes: the paginated open list and the
    per-key pulls/<n> reads for tracked keys the open list did not resolve."""

    def runner(cmd, **_kw):
        calls.append(list(cmd))
        path = cmd[2]
        if "/pulls?state=" in path:
            repo = path[len("repos/"): path.index("/pulls?")]
            pages = open_pages.get(repo)
            if pages is None:
                return Result(returncode=1, stdout="", stderr="network down")
            page_no = int(path.split("&page=")[1])
            if page_no > len(pages):
                return _page([])
            return _page(pages[page_no - 1])
        number = int(path.rsplit("/", 1)[-1])
        repo = path[len("repos/"): -len(f"/pulls/{number}")]
        row = per_key.get(f"{repo}#{number}")
        if row is None:
            return Result(returncode=1, stdout="", stderr="not found")
        return _ok(json.dumps(row))

    return runner


class TestTrackedStateSweepOnRest:
    def test_sweep_uses_rest_and_never_spawns_gh_pr_list(self):
        """AC1-HP: positive REST shape match plus an explicit absence of the
        old GraphQL verb, asserted across every argv the sweep issued."""
        from fno.pr_watch._discover import read_tracked_pr_states

        calls: list[list] = []
        runner = _sweep_runner(
            calls,
            open_pages={"owner/repo": [[_pr_row(1), _pr_row(2)]]},
            per_key={},
        )

        states, failures = read_tracked_pr_states({"owner/repo#1"}, runner=runner)

        assert failures == 0
        assert states == {"owner/repo#1": "OPEN", "owner/repo#2": "OPEN"}
        rest_calls = [c for c in calls if c[:2] == ["gh", "api"]]
        assert rest_calls, "no gh api call issued"
        assert all("/pulls" in c[2] for c in rest_calls)
        assert not any(_has_pr_list_pair(c) for c in calls)

    def test_open_list_paginates_into_page_two(self):
        """AC2-HP: a repo spanning more than one REST page returns every open
        PR; there is no truncation path left to hit."""
        from fno.pr_watch._discover import read_tracked_pr_states

        calls: list[list] = []
        runner = _sweep_runner(
            calls,
            open_pages={"owner/repo": [
                [_pr_row(n) for n in range(1, 101)],
                [_pr_row(100), _pr_row(101)],
            ]},
            per_key={},
        )

        states, failures = read_tracked_pr_states({"owner/repo#101"}, runner=runner)

        assert failures == 0
        assert states["owner/repo#100"] == "OPEN"
        assert states["owner/repo#101"] == "OPEN"
        pages = [c[2] for c in calls if "/pulls?state=" in c[2]]
        assert any("&page=2" in p for p in pages)

    def test_missing_tracked_key_resolved_by_one_per_key_read(self):
        """AC3-HP: absent from the open list -> exactly one pulls/<n> read,
        mapped through _map_pr_state to MERGED, never UNKNOWN."""
        from fno.pr_watch._discover import read_tracked_pr_states

        calls: list[list] = []
        runner = _sweep_runner(
            calls,
            open_pages={"owner/repo": [[_pr_row(7)]]},
            per_key={"owner/repo#9": {"number": 9, "state": "closed", "merged": True}},
        )

        states, failures = read_tracked_pr_states({"owner/repo#9"}, runner=runner)

        assert states == {"owner/repo#7": "OPEN", "owner/repo#9": "MERGED"}
        assert failures == 0
        per_key_calls = [c for c in calls if c[2].endswith("/pulls/9")]
        assert len(per_key_calls) == 1

    def test_missing_tracked_key_resolved_by_closed_batch_before_exact(self):
        """AC3-HP: a terminal key comes from the closed repository batch."""
        from fno.pr_watch._discover import read_tracked_pr_states

        calls: list[list] = []

        def runner(cmd, **_kw):
            calls.append(list(cmd))
            path = cmd[2]
            if "state=open" in path:
                return _page([])
            if "state=closed" in path:
                return _page([_pr_row(9, "closed", merged=True)])
            raise AssertionError(f"exact fallback should not run: {cmd}")

        states, failures = read_tracked_pr_states({"owner/repo#9"}, runner=runner)

        assert states == {"owner/repo#9": "MERGED"}
        assert failures == 0
        assert [call[2] for call in calls] == [
            "repos/owner/repo/pulls?state=open&per_page=100&page=1",
            "repos/owner/repo/pulls?state=closed&per_page=100&page=1",
        ]

    def test_failed_closed_batch_keeps_terminal_keys_unknown(self):
        """AC3-ERR: a failed terminal batch does not fabricate a state."""
        from fno.pr_watch._discover import read_tracked_pr_states

        calls: list[list] = []

        def runner(cmd, **_kw):
            calls.append(list(cmd))
            if "state=open" in cmd[2]:
                return _page([])
            return Result(returncode=1, stdout="", stderr="network down")

        states, failures = read_tracked_pr_states({"owner/repo#9"}, runner=runner)

        assert states == {"owner/repo#9": "UNKNOWN"}
        assert failures == 1
        assert len(calls) == 2

    def test_failed_repo_listing_degrades_with_failure_count(self):
        """AC4-EDGE: keys UNKNOWN (not deleted), sweep_failures counts the repo."""
        from fno.pr_watch._discover import read_tracked_pr_states

        calls: list[list] = []
        runner = _sweep_runner(calls, open_pages={}, per_key={})

        states, failures = read_tracked_pr_states(
            {"owner/repo#1", "owner/repo#2"}, runner=runner
        )

        assert states == {"owner/repo#1": "UNKNOWN", "owner/repo#2": "UNKNOWN"}
        assert failures == 1


class TestRestReaderHardening:
    def test_graphql_remaining_fails_open_when_the_runner_raises(self):
        """AC6-EDGE at the instrument: a runner that RAISES (gh off PATH
        raises ToolMissing) is an unreadable budget, not a tick-killing
        error. The contract is (None, None) on any failure."""
        from fno.pr._proc import ToolMissing
        from fno.pr._rest import graphql_remaining

        def raising_runner(cmd, **_kw):
            raise ToolMissing("gh")

        assert graphql_remaining(runner=raising_runner) == (None, None)

    def test_list_prs_rest_bounds_each_page_call_with_a_timeout(self):
        """The old gh pr list sweep killed each call at 30s; the REST port
        must not hand a black-holed page an unbounded runner."""
        from fno.pr._rest import list_prs_rest

        seen = {}

        def runner(cmd, timeout=None, **_kw):
            seen["timeout"] = timeout
            return _page([])

        rows, reason = list_prs_rest("owner/repo", runner=runner)
        assert rows == []
        assert reason == ""
        assert seen["timeout"] is not None and seen["timeout"] > 0

    def test_sweep_degrades_a_repo_whose_runner_raises_toolmissing(self):
        """launchd PATH rot makes gh vanish; ToolMissing is not OSError, so
        the per-repo handler must name it like any other repo failure."""
        from fno.pr._proc import ToolMissing
        from fno.pr_watch._discover import read_tracked_pr_states

        def runner(cmd, **_kw):
            raise ToolMissing("gh")

        states, failures = read_tracked_pr_states({"owner/repo#1"}, runner=runner)
        assert states == {"owner/repo#1": "UNKNOWN"}
        assert failures == 1

    def test_failed_per_key_reads_count_toward_the_failure_total(self):
        """A listing that succeeds while every per-key read fails must not
        read as a clean sweep: outcome ok with unresolved keys is the
        swallowed-failure shape AC4 exists to end."""
        from fno.pr_watch._discover import read_tracked_pr_states

        calls: list[list] = []
        runner = _sweep_runner(calls, open_pages={"owner/repo": []}, per_key={})

        states, failures = read_tracked_pr_states({"owner/repo#1"}, runner=runner)

        assert states == {"owner/repo#1": "UNKNOWN"}
        assert failures == 1

    def test_exact_terminal_fallback_is_capped_and_timeout_bounded(self):
        """AC4-HP/ERR: exact reads stop at the cap and never exceed five seconds."""
        from fno.pr_watch._discover import (
            EXACT_TERMINAL_READ_TIMEOUT_S,
            MAX_EXACT_TERMINAL_READS,
            read_tracked_pr_states,
        )

        keys = {f"owner/repo#{number}" for number in range(1, MAX_EXACT_TERMINAL_READS + 3)}
        exact_calls: list[list] = []
        exact_timeouts: list[float] = []

        def runner(cmd, timeout=None, **_kw):
            path = cmd[2]
            if "state=open" in path or "state=closed" in path:
                return _page([])
            exact_calls.append(list(cmd))
            exact_timeouts.append(timeout)
            raise subprocess.TimeoutExpired(cmd, timeout)

        states, failures = read_tracked_pr_states(keys, runner=runner)

        assert states == {key: "UNKNOWN" for key in keys}
        assert len(exact_calls) == MAX_EXACT_TERMINAL_READS
        assert exact_timeouts == [EXACT_TERMINAL_READ_TIMEOUT_S] * MAX_EXACT_TERMINAL_READS
        assert failures >= len(keys)

    def test_graphql_remaining_bounds_the_subprocess_wait(self):
        """The preflight read must not hand a black-holed gh an unbounded
        wait: this PR exists to end tick hangs, not to add one."""
        from fno.pr._rest import graphql_remaining

        seen = {}

        def runner(cmd, timeout=None, **_kw):
            seen["timeout"] = timeout
            return _ok(
                json.dumps({"resources": {"graphql": {"remaining": 5, "reset": 1755433200}}})
            )

        remaining, _reset = graphql_remaining(runner=runner)
        assert remaining == 5
        assert seen["timeout"] is not None and seen["timeout"] > 0

    def test_per_key_payload_that_is_not_an_object_degrades_one_key(self):
        """gh output drift can return rc 0 with a JSON array; _map_pr_state
        would raise AttributeError out of the whole sweep instead of one
        key failing."""
        from fno.pr_watch._discover import read_tracked_pr_states

        calls: list[list] = []
        runner = _sweep_runner(calls, open_pages={"owner/repo": []}, per_key={"owner/repo#1": [1]})

        states, failures = read_tracked_pr_states({"owner/repo#1"}, runner=runner)

        assert states == {"owner/repo#1": "UNKNOWN"}
        assert failures == 1

    def test_sweep_passes_the_caller_timeout_to_the_listing(self):
        """timeout_s is the caller's bound on the sweep's gh calls; the REST
        listing must honor it, not silently run on its own default."""
        from fno.pr_watch._discover import read_tracked_pr_states

        seen: dict[str, object] = {}

        def runner(cmd, timeout=None, **_kw):
            if "/pulls?state=" in cmd[2]:
                seen["listing_timeout"] = timeout
                return _page([])
            return _ok("{}")

        read_tracked_pr_states({"owner/repo#1"}, runner=runner, timeout_s=5.0)

        assert seen["listing_timeout"] == 5.0

    def test_full_listing_at_the_page_ceiling_warns_loudly(self, caplog):
        """The ceiling is a truncation, not a completion: a listing whose
        every page came back full must say so, the way the old gh pr list
        path logged 'possibly truncated'."""
        import logging as _logging

        from fno.pr._rest import list_prs_rest

        full_page = [_pr_row(i) for i in range(100)]
        calls = {"pages": 0}

        def runner(cmd, timeout=None, **_kw):
            calls["pages"] += 1
            return _page(full_page)

        with caplog.at_level(_logging.WARNING, logger="fno.pr._rest"):
            rows, reason = list_prs_rest(
                "owner/repo", runner=runner, per_page=100, max_pages=2
            )

        assert reason == ""
        assert len(rows) == 200
        assert calls["pages"] == 2
        assert "possibly truncated" in caplog.text
