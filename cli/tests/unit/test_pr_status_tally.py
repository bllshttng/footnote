"""The tally-conflation contract for `fno pr status` (x-d7be).

`statusCheckRollup` carries two different kinds of row: GitHub check-runs (the
`name` key, produced by Actions and Checks-API apps) and commit StatusContexts
(the `context` key, posted to the statuses endpoint). `checks.total` counts
both. A reader who compares that total against
`gh api repos/O/R/commits/<sha>/check-runs` therefore sees a gap that looks
like a missing job, because that endpoint never returns statuses.

These tests pin the SPLIT, not the total. The tally is one refactor away from
being re-merged by whoever next reads the rollup as one list, and the fix
without the test is that refactor's invitation.
"""
from __future__ import annotations

from fno.pr import _status


def test_tally_splits_check_runs_from_statuses():
    """The PR 994 specimen, restated: 13 check-runs + 2 statuses read as 15.

    Measured 2026-08-20. `fno pr status` said 15 checks; GitHub's check-runs
    endpoint named 13 (12 success, 1 skipped); the two extra rows were fno's
    own statuses, stacked-base-guard and fno/review-coverage. Both numbers
    were right about different questions, and nothing in the payload said so.
    """
    rollup = [
        {"name": "job-" + str(i), "status": "COMPLETED", "conclusion": "SUCCESS"}
        for i in range(12)
    ]
    rollup.append({"name": "skipped-job", "status": "COMPLETED", "conclusion": "SKIPPED"})
    rollup.append({"context": "stacked-base-guard", "state": "SUCCESS"})
    rollup.append({"context": "fno/review-coverage", "state": "SUCCESS"})

    verdict, _code, counts = _status.verdict_for(rollup)
    assert verdict == "green"
    assert counts["total"] == 15
    # What a direct check-runs read answers with.
    assert counts["check_runs"] == 13
    # The rows that endpoint never returns.
    assert counts["statuses"] == 2
    assert counts["check_runs"] + counts["statuses"] == counts["total"]


def test_a_status_only_rollup_reports_zero_check_runs():
    """The x-4271 shape, now legible in the tally and not only in the verdict.

    A dirty PR gets no workflow runs at all, but fno's own statuses still post
    and can all pass. The verdict already refuses that green; the tally must
    also SAY that nothing rode the check-runs API, so a reader sees the why.
    """
    rollup = [
        {"context": "stacked-base-guard", "state": "SUCCESS"},
        {"context": "fno/review-coverage", "state": "SUCCESS"},
    ]
    verdict, _code, counts = _status.verdict_for(rollup)
    assert verdict == "unknown"
    assert counts["total"] == 2
    assert counts["check_runs"] == 0
    assert counts["statuses"] == 2


def test_a_checkrun_and_a_status_sharing_a_name_are_counted_separately():
    """Dedup keys on (kind, name), so the split must not re-merge them.

    Two rows with the same literal string are two different checks. Counting
    them as one check-run would restate the conflation inside the fix.
    """
    rollup = [
        {"name": "build", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"context": "build", "state": "SUCCESS"},
    ]
    _verdict, _code, counts = _status.verdict_for(rollup)
    assert counts["total"] == 2
    assert counts["check_runs"] == 1
    assert counts["statuses"] == 1


def test_the_split_never_invents_a_row_for_an_unkeyed_entry():
    """A rollup row with neither key belongs to neither sub-count.

    The sub-counts may sum to less than `total`. They may never sum to more,
    which would invent a job that no endpoint would confirm.
    """
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"state": "SUCCESS"},
    ]
    _verdict, _code, counts = _status.verdict_for(rollup)
    assert counts["total"] == 2
    assert counts["check_runs"] == 1
    assert counts["statuses"] == 0
