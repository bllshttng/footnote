"""The tally-conflation contract for `fno do pr status` (x-d7be).

`statusCheckRollup` carries two different kinds of row: GitHub check-runs (the
`name` key, produced by Actions and Checks-API apps) and commit StatusContexts
(the `context` key, posted to the statuses endpoint). `checks.total` counts
both. A reader who compares that total against
`gh api repos/O/R/commits/<sha>/check-runs` therefore sees a gap that looks
like a missing job, because that endpoint never returns statuses.

These tests pin the SPLIT, not the total. The tally is one refactor away from
being re-merged by whoever next reads the rollup as one list, and the fix
without the test is that refactor's invitation.

The second half pins the VERDICT layer. Counting the two kinds apart in the
report is worth nothing if the branch that names a red still reads one
combined number, which is where the split first landed and where a reader
actually acts on it.
"""
from __future__ import annotations

import pytest

from fno.pr import _status


@pytest.fixture(autouse=True)
def _no_dispatch_hold(monkeypatch):
    monkeypatch.setattr(_status, "_merge_hold_reason", lambda pr, cwd: None)


def test_tally_splits_check_runs_from_statuses():
    """The PR 994 specimen, restated: 13 check-runs + 2 statuses read as 15.

    Measured 2026-08-20. `fno do pr status` said 15 checks; GitHub's check-runs
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


# --- the verdict layer -----------------------------------------------------
#
# The split above lives in the REPORT. These pin the BRANCH. `verdict_for`
# returns red off the combined fail count and `_ready_blockers` names it, so
# a fix that stops at the counts leaves the two kinds conflated exactly where
# a reader acts on them. The blocker name is what got read as "CI red" on a
# head whose every job passed.
#
# The verdict itself deliberately does NOT split. A failing StatusContext is a
# real red and must never read green.


def _blockers(rollup, coverage=None):
    """The ready_blockers a rollup produces, with everything else passing."""
    verdict, _code, counts = _status.verdict_for(rollup)
    return _status._ready_blockers(
        verdict == "green",
        verdict,
        0,
        coverage or {"coverage": "covered", "reviewed_count": 2},
        True,
        head="h" * 40,
        counts=counts,
    )


def test_a_failing_job_is_still_named_ci_red():
    """The unchanged case. A real check-run failed, so CI really is red."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"},
        {"context": "fno/review-coverage", "state": "SUCCESS"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert (verdict, code) == ("red", 1)
    assert counts["fail_check_runs"] == 1
    assert "ci_red" in _blockers(rollup)


def test_a_status_only_red_is_never_named_ci_red():
    """The PR 997 shape. Every job passed and a commit status failed.

    `ci_red` here is a claim a reader acts on and it is false: it woke a
    session with "CI red, fno/review-coverage failed", which is two
    incompatible statements in one line.
    """
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"context": "fno/review-coverage", "state": "FAILURE"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    # Still red, still exit 1. The verdict is not what was wrong.
    assert (verdict, code) == ("red", 1)
    assert counts["fail_check_runs"] == 0
    assert counts["fail_statuses"] == 1
    blockers = _blockers(rollup)
    assert "commit_status_red" in blockers
    assert "ci_red" not in blockers


def test_a_status_only_red_is_renamed_never_dropped():
    """Rename, never remove - the reason the fix is not a de-duplication.

    `stacked-base-guard` is also a StatusContext, and no coverage conjunct
    names it. Dropping the blocker on a status-only red to de-duplicate the
    coverage case would silence the only name this gate ever gets.
    """
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"context": "stacked-base-guard", "state": "FAILURE"},
    ]
    blockers = _blockers(rollup)
    assert blockers == ["commit_status_red"], blockers


def test_a_failing_job_beside_a_failing_status_stays_ci_red():
    """A job really did fail, so the honest name is the CI one.

    The predicate is 'no job failed', never 'a status failed' - otherwise one
    red status relabels a genuinely broken build.
    """
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"},
        {"context": "fno/review-coverage", "state": "FAILURE"},
    ]
    _verdict, _code, counts = _status.verdict_for(rollup)
    assert counts["fail_check_runs"] == 1
    assert counts["fail_statuses"] == 1
    assert "ci_red" in _blockers(rollup)


def test_an_unattributable_red_is_never_named_a_commit_status():
    """The name asserts a POSITIVE fact, never the absence of a failing job.

    A rollup row with neither key still classifies as a fail and lands in
    neither sub-count, so `fail_check_runs == 0` on its own is two different
    situations: a status failed, or nothing nameable did. Reading the absence
    named a `commit_status_red` on a rollup carrying no status at all.
    """
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"state": "FAILURE"},
    ]
    _verdict, _code, counts = _status.verdict_for(rollup)
    assert counts["fail"] == 1
    assert counts["fail_check_runs"] == 0 and counts["fail_statuses"] == 0
    blockers = _blockers(rollup)
    assert "commit_status_red" not in blockers, blockers
    assert "ci_red" in blockers


def test_pending_and_unknown_keep_their_ci_names():
    """Only a RED splits. A pending or unknown read is not an attribution
    problem, and renaming it would churn a string for nothing."""
    pending = [{"name": "ci", "status": "IN_PROGRESS", "conclusion": ""}]
    assert "ci_pending" in _blockers(pending)
    assert "ci_unknown" in _blockers([])


def test_a_status_red_beside_an_unattributable_red_keeps_the_ci_name():
    """`not fail_check_runs` is still an ABSENCE when an unkeyed row failed.

    A failing status beside a failing row that carries neither key satisfies
    "no job failed" while a second, un-nameable red is also holding. Naming
    that `commit_status_red` tells a reader the only thing wrong is a status
    they can go look at. The honest test is that EVERY failing row is a status.
    """
    rollup = [
        {"context": "fno/review-coverage", "state": "FAILURE"},
        {"state": "FAILURE"},
    ]
    _verdict, _code, counts = _status.verdict_for(rollup)
    assert counts["fail"] == 2
    assert counts["fail_statuses"] == 1 and counts["fail_check_runs"] == 0
    blockers = _blockers(rollup)
    assert "commit_status_red" not in blockers, blockers
    assert "ci_red" in blockers


def test_an_extra_positional_is_refused_never_dropped():
    """`main(["42","43"])` answered for 42 and discarded 43 in silence.

    Same shape as the unknown flag one line above it in the parser: a caller
    asked something the parser did not answer, and got no signal back.
    """
    assert _status.main(["42", "43"]) == 2
    assert _status.main([]) == 2
