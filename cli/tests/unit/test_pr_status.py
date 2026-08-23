"""Tests for `fno do pr status` verdict logic (x-8b64 G).

The non-trivial part is classifying a mixed statusCheckRollup: CheckRun entries
carry status+conclusion (conclusion empty until COMPLETED), StatusContext
entries carry only state. The Boundary cases: an in-progress check is *pending*
not red, and an empty rollup is *unknown* not red.
"""
from __future__ import annotations

import json as _json
import subprocess

import pytest

from fno.pr import _reviews, _status
from fno.pr._proc import Result

# Captured at import, before the autouse stub below replaces the attribute:
# the two tests that exercise the fail-closed wrapper need the real one.
_REAL_REVIEW_ACTIVITY = _status._review_activity


@pytest.fixture(autouse=True)
def _no_dispatch_hold(monkeypatch):
    monkeypatch.setattr(_status, "_merge_hold_reason", lambda pr, cwd: None)


@pytest.fixture(autouse=True)
def _no_review_activity(monkeypatch):
    """Neutralize the in-flight-review probe unless a test steers it.

    Its worktree layer shells `git`, and a status test must never read the
    machine's real worktrees. Tests that exercise the conjunct patch it back.
    """
    from fno.pr._review_hold import ReviewActivity

    monkeypatch.setattr(
        _status,
        "_review_activity",
        lambda branch, head, cwd: ReviewActivity(
            False, "", "", None,
            {"probed": True, "path": None, "dirty": None, "head": None, "note": "stubbed"},
        ),
    )


def test_all_pass_is_green():
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"context": "legacy", "state": "SUCCESS"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "green"
    assert code == 0
    assert counts == {
        "total": 2,
        "check_runs": 1,
        "statuses": 1,
        "fail_check_runs": 0,
        "fail_statuses": 0,
        "pass": 2,
        "fail": 0,
        "pending": 0,
        "unsettled": 0,
        "unsettled_fail": 0,
    }


def test_any_failure_is_red():
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"name": "lint", "status": "COMPLETED", "conclusion": "FAILURE"},
    ]
    verdict, code, _ = _status.verdict_for(rollup)
    assert verdict == "red"
    assert code == 1


def test_in_progress_check_is_pending_not_red():
    """Boundary: a CheckRun with status=IN_PROGRESS has conclusion='' and must
    read as pending, never red."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"name": "build", "status": "IN_PROGRESS", "conclusion": ""},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "pending"
    assert code == 2
    assert counts["pending"] == 1


def test_status_context_pending_is_pending():
    rollup = [{"context": "deploy", "state": "PENDING"}]
    verdict, code, _ = _status.verdict_for(rollup)
    assert verdict == "pending"
    assert code == 2


def test_no_checks_is_unknown_not_red():
    """Boundary: a PR with no checks -> unknown, not red."""
    verdict, code, counts = _status.verdict_for([])
    assert verdict == "unknown"
    assert code == 3
    assert counts["total"] == 0


def test_failure_wins_over_pending():
    rollup = [
        {"name": "build", "status": "IN_PROGRESS", "conclusion": ""},
        {"name": "lint", "status": "COMPLETED", "conclusion": "FAILURE"},
    ]
    verdict, code, _ = _status.verdict_for(rollup)
    assert verdict == "red"
    assert code == 1


# ── x-def4: latest-run-per-name dedup ────────────────────────────────────────
# A force/amend push leaves superseded runs (e.g. a CANCELLED CI) in the rollup
# beside the fresh ones. verdict_for must classify only the latest run per name
# so a superseded CANCELLED loses to a newer SUCCESS - WITHOUT hiding a genuine
# cancel that IS the latest run.


def test_superseded_cancelled_loses_to_newer_success_is_green():
    """AC1: ci CANCELLED (earlier attempt) + ci SUCCESS (later attempt) + other
    HEAD checks SUCCESS -> green, fail count 0."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "CANCELLED",
         "startedAt": "2026-07-09T09:55:00Z", "completedAt": "2026-07-09T10:00:00Z"},
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS",
         "startedAt": "2026-07-09T10:00:00Z", "completedAt": "2026-07-09T10:05:00Z"},
        {"name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS",
         "startedAt": "2026-07-09T10:00:00Z", "completedAt": "2026-07-09T10:05:00Z"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "green"
    assert code == 0
    assert counts["fail"] == 0
    # total reflects the deduped set (honest check count), not the raw rollup.
    assert counts["total"] == 2


def test_stale_run_completing_later_does_not_hide_latest_fail():
    """Invariant regression: the latest ATTEMPT is the one that STARTED last,
    even when a stale superseded run completes later. A fast-failing current
    run must not be masked by a slow stale SUCCESS - keying on startedAt (not
    completedAt) keeps the genuine fail."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE",
         "startedAt": "2026-07-09T10:01:00Z", "completedAt": "2026-07-09T10:02:00Z"},
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS",
         "startedAt": "2026-07-09T09:50:00Z", "completedAt": "2026-07-09T10:03:00Z"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "red"
    assert code == 1
    assert counts["total"] == 1


def test_checkrun_and_statuscontext_same_name_not_merged():
    """Invariant regression: a CheckRun `name` and a StatusContext `context`
    sharing a literal string are DIFFERENT checks and must not be merged - the
    StatusContext FAILURE must survive beside the newer CheckRun and force red."""
    rollup = [
        {"context": "ci", "state": "FAILURE", "createdAt": "2026-07-09T10:00:00Z"},
        {"name": "ci", "status": "IN_PROGRESS", "conclusion": "",
         "startedAt": "2026-07-09T10:05:00Z"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "red"
    assert code == 1
    assert counts["total"] == 2


def test_genuine_latest_cancel_stays_red():
    """AC2: a single ci CANCELLED run with no newer same-name run -> red
    (the invariant: filtering must not hide a genuine fail)."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "CANCELLED",
         "completedAt": "2026-07-09T10:00:00Z"},
    ]
    verdict, code, _ = _status.verdict_for(rollup)
    assert verdict == "red"
    assert code == 1


# ── x-1d38: settled requires a positive marker, never absent pending ─────────
# `settled` answers "is there anything left to wait for", which is a DIFFERENT
# question than the verdict. It must be derived from a positive settled marker
# per latest run, so an empty rollup (no runs yet) and a cancelled latest run
# (a result that was taken away) can never read as decided.


def _run_status_on(monkeypatch, capsys, rollup):
    """run_status with gh stubbed out; returns (exit code, parsed JSON, stderr)."""

    def _patch(name, value):
        monkeypatch.setattr(_status, name, value)

    _patch("_fetch", lambda pr, cwd: ({"state": "OPEN", "statusCheckRollup": rollup}, ""))
    _patch(
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    _patch(
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "unknown", "reviewed_count": None},
    )
    # Same reason as _green_fetch: never the machine's own lane config, which
    # can reach a live `gh pr view` through the self-review floor.
    _patch("_review_lane", lambda pr, cwd: True)
    code = _status.run_status("42")
    cap = capsys.readouterr()
    return code, _json.loads(cap.out), cap.err


def test_ac1_cancelled_latest_is_red_and_unsettled(monkeypatch, capsys):
    """AC1-HP: a genuinely-cancelled latest run keeps verdict red (a cancelled
    run is not a pass) but settled FALSE (it is not a conclusion either - the
    run was taken away, nothing was decided). Exit code stays 1."""
    code, out, err = _run_status_on(
        monkeypatch,
        capsys,
        [{"name": "ci", "status": "COMPLETED", "conclusion": "CANCELLED"}],
    )
    assert code == 1
    assert out["verdict"] == "red"
    assert out["settled"] is False
    assert out["checks"]["unsettled"] == 1
    assert "ci_cancelled_retrigger" in out["ready_blockers"]
    assert "ci_red" not in out["ready_blockers"]
    # The instruction travels with the number: which check, and what to do.
    assert "ci" in err
    assert "do not read this pr as decided" in err.lower()


def test_ac2_all_concluded_passes_stay_settled(monkeypatch, capsys):
    """AC2-HP: every latest run carrying a real pass conclusion (SUCCESS,
    NEUTRAL, SKIPPED) stays settled - unchanged from before the change."""
    code, out, err = _run_status_on(
        monkeypatch,
        capsys,
        [
            {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "lint", "status": "COMPLETED", "conclusion": "SKIPPED"},
            {"context": "legacy", "state": "NEUTRAL"},
        ],
    )
    assert code == 0
    assert out["settled"] is True
    assert out["checks"]["unsettled"] == 0
    # The human verdict line is the only stderr; no note fires on a
    # settled green PR.
    assert err == _status.verdict_line(out) + "\n"


def test_ac3_empty_rollup_is_never_settled(monkeypatch, capsys):
    """AC3-EDGE: no runs at all -> unknown verdict AND settled false. Zero
    pending entries must not read as all-green: an absence has two
    explanations and only one of them is decided."""
    code, out, err = _run_status_on(monkeypatch, capsys, [])
    assert code == 3
    assert out["verdict"] == "unknown"
    assert out["settled"] is False
    # The human verdict line is the only stderr; an empty rollup has no
    # check to advise about.
    assert err == _status.verdict_line(out) + "\n"


def test_ac4_superseded_cancelled_settles_green(monkeypatch, capsys):
    """AC4-EDGE: a superseded CANCELLED run loses to the newer same-name
    SUCCESS (existing dedup) and the result is fully settled green."""
    code, out, _ = _run_status_on(
        monkeypatch,
        capsys,
        [
            {"name": "ci", "status": "COMPLETED", "conclusion": "CANCELLED",
             "startedAt": "2026-08-15T00:00:00Z"},
            {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS",
             "startedAt": "2026-08-15T01:00:00Z"},
        ],
    )
    assert code == 0
    assert out["verdict"] == "green"
    assert out["settled"] is True
    assert out["checks"]["unsettled"] == 0


def test_ac5_failure_is_a_real_result_and_settles(monkeypatch, capsys):
    """AC5-ERR: a FAILURE conclusion settles. This is the boundary that proves
    the change is about absence, not redness: red AND settled when the red
    comes from a run that actually produced a result."""
    code, out, _ = _run_status_on(
        monkeypatch,
        capsys,
        [{"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"}],
    )
    assert code == 1
    assert out["verdict"] == "red"
    assert out["settled"] is True
    assert out["checks"]["unsettled"] == 0


def test_pending_check_gets_a_wait_note_never_a_push_again_note(monkeypatch, capsys):
    """codex P2: an ordinary in-progress check (status QUEUED, no conclusion
    yet) is unsettled but was NOT cancelled or stale. The note must say
    "still queued or running" and "wait", never "cancelled or stale" and
    "push again" - that instruction belongs to a completed-but-markerless
    entry alone. The verdict in the note must read the real pending verdict,
    never a hardcoded red."""
    code, out, err = _run_status_on(
        monkeypatch,
        capsys,
        [{"name": "ci", "status": "QUEUED", "conclusion": None}],
    )
    assert code == 2
    assert out["verdict"] == "pending"
    assert out["settled"] is False
    assert "still queued or running" in err
    assert "wait for the run" in err.lower()
    assert "cancelled or stale" not in err
    assert "push again" not in err.lower()


def test_pending_and_cancelled_get_their_own_distinct_notes(monkeypatch, capsys):
    """Both causes can coexist in one rollup and must not blend into one
    instruction: the cancelled check says push again, the pending check
    says wait, and each name appears only in its own note."""
    code, out, err = _run_status_on(
        monkeypatch,
        capsys,
        [
            {"name": "ci", "status": "COMPLETED", "conclusion": "CANCELLED"},
            {"name": "smoke", "status": "IN_PROGRESS", "conclusion": None},
        ],
    )
    assert code == 1
    assert out["checks"]["unsettled"] == 2
    assert "cancelled or stale" in err
    assert "still queued or running" in err
    # The verdict line is always the first stderr line; the notes follow it.
    assert err.split("\n", 1)[0] == _status.verdict_line(out)
    notes = err.split("\n", 1)[1]
    cancelled_note, pending_note = notes.split("\n", 1)
    assert "ci" in cancelled_note and "smoke" not in cancelled_note
    assert "smoke" in pending_note and "ci" not in pending_note


def test_latest_in_progress_over_earlier_success_is_pending():
    """AC3: latest ci run IN_PROGRESS (empty conclusion, startedAt after the
    earlier ci SUCCESS completed) -> pending, never green or red."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS",
         "startedAt": "2026-07-09T10:00:00Z", "completedAt": "2026-07-09T10:03:00Z"},
        {"name": "ci", "status": "IN_PROGRESS", "conclusion": "",
         "startedAt": "2026-07-09T10:05:00Z"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "pending"
    assert code == 2
    assert counts["pending"] == 1
    assert counts["total"] == 1


def test_dedup_empty_rollup_is_unknown():
    """AC4: empty rollup stays unknown (unchanged)."""
    verdict, code, counts = _status.verdict_for([])
    assert verdict == "unknown"
    assert code == 3
    assert counts["total"] == 0


def test_all_pass_status_contexts_with_zero_check_runs_is_unknown():
    """x-4271: a conflicting PR (mergeable_state dirty) gets zero workflow
    runs, but fno's own self-published StatusContexts (review-coverage,
    stacked-base-guard) still post and can all pass. An all-`context` rollup
    with no real check-run must read unknown, never green - PR 965 read
    green ready true off exactly this shape with zero CI having run."""
    rollup = [
        {"context": "fno/review-coverage", "state": "SUCCESS"},
        {"context": "stacked-base-guard", "state": "SUCCESS"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "unknown"
    assert code == 3
    assert counts["total"] == 2


def test_a_failing_status_context_still_reds_with_zero_check_runs():
    """A StatusContext-only rollup that is genuinely failing is still a real,
    actionable signal - the zero-real-check-run refusal only intercepts a
    would-be-green tally, never masks a fail as unknown."""
    rollup = [{"context": "fno/review-coverage", "state": "FAILURE"}]
    verdict, code, _ = _status.verdict_for(rollup)
    assert verdict == "red"
    assert code == 1


def test_dedup_single_entry_per_name_is_unchanged():
    """Boundary: one entry per name -> nothing to dedup, behaves as today."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS",
         "completedAt": "2026-07-09T10:00:00Z"},
        {"context": "legacy", "state": "SUCCESS"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "green"
    assert counts["total"] == 2


def test_timestampless_tie_is_fail_closed_pass_then_fail():
    """Tie (both timestampless): a FAILURE seen after a SUCCESS of the same name
    keeps the fail -> red."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"},
    ]
    verdict, _, counts = _status.verdict_for(rollup)
    assert verdict == "red"
    assert counts["total"] == 1


def test_timestampless_tie_is_fail_closed_fail_then_pass():
    """Invariant regression (codex peer, x-8332): a FAILURE seen BEFORE a
    same-name SUCCESS with equal/missing timestamps must NOT be dropped. The old
    `>=` last-seen rule returned green here (hid the fail); fail-closed -> red."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"},
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "red"
    assert code == 1
    assert counts["total"] == 1


def test_newer_success_before_older_superseded_fail_is_green():
    """Regression (gemini HIGH / codex P1 on PR #316): the fail-preservation
    branch must fire ONLY on a true tie. A newer SUCCESS seen BEFORE an older
    superseded FAILURE of the same name must keep the SUCCESS -> green; the
    strictly-older fail loses (else it re-hides as a false red, the x-e858 bug)."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS",
         "startedAt": "2026-07-09T10:05:00Z"},
        {"name": "ci", "status": "COMPLETED", "conclusion": "CANCELLED",
         "startedAt": "2026-07-09T10:00:00Z"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "green"
    assert code == 0
    assert counts["fail"] == 0
    assert counts["total"] == 1


def test_pr966_style_rerun_failure_loses_to_newer_success():
    """x-5a83: PR 966 head 8c0a736e carried two check-runs named
    check-pr-body-style, an 05:09Z FAILURE and a 12:37Z SUCCESS rerun. The
    tally took the failure and a green PR read red. Pinned with the REST
    reader's actual (lowercase status/conclusion) shape, not the GraphQL
    uppercase one every other test in this file uses."""
    rollup = [
        {"name": "check-pr-body-style", "status": "completed", "conclusion": "failure",
         "startedAt": "2026-08-19T05:09:34Z"},
        {"name": "check-pr-body-style", "status": "completed", "conclusion": "success",
         "startedAt": "2026-08-19T12:37:59Z"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "green"
    assert code == 0
    assert counts["fail"] == 0
    assert counts["total"] == 1


def test_equal_timestamp_tie_preserves_fail():
    """Tie on EQUAL (present) startedAt: a same-name FAILURE before a SUCCESS is
    preserved -> red, regardless of order."""
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE",
         "startedAt": "2026-07-09T10:00:00Z"},
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS",
         "startedAt": "2026-07-09T10:00:00Z"},
    ]
    verdict, _, counts = _status.verdict_for(rollup)
    assert verdict == "red"
    assert counts["total"] == 1


def test_unresolved_counter_tells_you_a_reply_is_not_a_resolve(monkeypatch, capsys):
    """The counter alone cannot distinguish "not answered" from "answered but not
    resolved", and the second state reads as handled while holding ready at false.
    So the instruction travels with the number, on stderr so the JSON contract is
    untouched. A session lost time to this; a PR body would not have reached it.
    """
    monkeypatch.setattr(
        _status, "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status, "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 2},
    )
    monkeypatch.setattr(
        _status, "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "unknown", "reviewed_count": None},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    _status.run_status("42")
    cap = capsys.readouterr()
    import json

    assert json.loads(cap.out)["ready"] is False, "stdout stays pure JSON"
    assert "REPLY DOES NOT RESOLVE" in cap.err
    assert "resolveReviewThread" in cap.err, "name the mutation, not just the problem"


def test_no_resolve_hint_when_nothing_is_unresolved(monkeypatch, capsys):
    """The hint is advice, not decoration: silent when there is nothing to do."""
    monkeypatch.setattr(
        _status, "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status, "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    monkeypatch.setattr(
        _status, "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "unknown", "reviewed_count": None},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    _status.run_status("42")
    cap = capsys.readouterr()
    # The human verdict line is the only stderr a clean PR prints; every
    # note is advice for a state this PR is not in.
    assert cap.err == _status.verdict_line(_json.loads(cap.out)) + "\n"


# ---- the human verdict line: one line, fixed slots, conclusion last ----


def _line_payload(**over):
    """A clean payload as run_status emits it, overridable per variant."""
    head = "3e3b45d4bc24" + "0" * 28
    payload = {
        "pr": "1043",
        "head": head,
        "verdict": "green",
        "settled": True,
        "green": True,
        "pr_state": "OPEN",
        "mergeable": "MERGEABLE",
        "checks": {"unsettled": 0, "total": 2},
        "ready": True,
        "ready_blockers": [],
        "review_coverage": {"coverage": "covered", "head_sha": head},
    }
    payload.update(over)
    return payload


def test_verdict_line_clean_pr_is_one_quiet_line():
    """AC1-HP: every slot of a clean PR reads as a quiet lowercase word and
    the conclusion clause comes last, so the lazy read is the complete read."""
    assert _status.verdict_line(_line_payload()) == (
        "1043 OPEN green settled mergeable ready @ 3e3b45d4bc24 - no blockers"
    )


def test_verdict_line_every_slot_matters():
    """AC3-ERR: changing any one of the slots a misreader omitted must change
    the line, and no two varied payloads may render the same line - a slot
    that never changes the output is a decorative field, and the reader who
    budgets for it is worse off than one who never saw it."""
    base = _line_payload()
    variants = [
        _line_payload(pr_state="CLOSED"),
        _line_payload(verdict="red", green=False, ready=False,
                      ready_blockers=["ci_cancelled_retrigger"]),
        _line_payload(settled=False, checks={"unsettled": 2, "total": 2}),
        _line_payload(mergeable="UNKNOWN"),
        _line_payload(ready=False, ready_blockers=["optional_reviews_unresolved"]),
    ]
    base_line = _status.verdict_line(base)
    for payload in variants:
        assert _status.verdict_line(payload) != base_line
    lines = [base_line] + [_status.verdict_line(p) for p in variants]
    assert len(set(lines)) == len(lines)


def test_verdict_line_marks_every_wrong_value_loud():
    """AC3-ERR: a non-clean value renders a visibly marked token - capitals
    or a parenthetical - so scanning for trouble is a real strategy."""
    line = _status.verdict_line(_line_payload(
        pr_state="CLOSED",
        verdict="red",
        green=False,
        settled=False,
        checks={"unsettled": 2, "total": 2},
        mergeable="CONFLICTING",
        ready=False,
        ready_blockers=["not_mergeable_conflicting", "ci_red"],
    ))
    assert "CLOSED" in line
    assert "unsettled(2)" in line
    assert "CONFLICTING" in line
    assert "NOT-ready" in line
    assert line.endswith("- 2 blockers: not_mergeable_conflicting, ci_red")


def test_verdict_line_spells_out_mergeable_unknown():
    """The fourth misreading: a not-yet-computed answer arrives as the string
    UNKNOWN, and the line states its meaning instead of asking the reader to
    interpret a word - or worse, a null."""
    assert "mergeable-unknown(not-yet-computed)" in _status.verdict_line(
        _line_payload(mergeable="UNKNOWN")
    )


def test_verdict_line_distinguishes_unavailable_from_computing():
    """An absent `mergeable` is a different fact from an explicit UNKNOWN:
    the error payload and the terminal-PR arm never asked GitHub, so
    "not yet computed" would claim a computation nothing is running."""
    unavailable = _status.verdict_line(_line_payload(mergeable=None))
    computing = _status.verdict_line(_line_payload(mergeable="UNKNOWN"))
    assert "mergeable-unavailable(no-answer)" in unavailable
    assert unavailable != computing


def test_verdict_line_carries_a_head_mismatch_without_arithmetic():
    """AC4-ERR: coverage computed at another commit renders both commits at
    12 characters in one line. A matching coverage head renders nothing, so
    the parenthetical's presence is itself the signal."""
    mismatched = _line_payload()
    mismatched["review_coverage"] = {
        "coverage": "covered", "head_sha": "1a2b3c4d5e6f" + "0" * 28,
    }
    assert (
        "@ 3e3b45d4bc24 (coverage at 1a2b3c4d5e6f) - "
        in _status.verdict_line(mismatched)
    )
    assert "(coverage at" not in _status.verdict_line(_line_payload())


def test_run_status_keeps_stdout_a_pure_json_contract(monkeypatch, capsys):
    """AC2-HP: stdout carries exactly one JSON object with the pre-change
    keys, and the human line lands on stderr - the fleet's watcher greps
    stdout with stderr discarded, so the two streams must never trade."""
    monkeypatch.setattr(
        _status, "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "headRefOid": "3e3b45d4bc24" + "0" * 28,
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [
                {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}
            ],
        }, ""),
    )
    monkeypatch.setattr(
        _status, "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    monkeypatch.setattr(
        _status, "read_review_coverage",
        lambda pr, cwd, **kw: {
            "coverage": "unknown",
            "reviewed_count": None,
            "head_sha": "3e3b45d4bc24" + "0" * 28,
        },
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    code = _status.run_status("1043")
    cap = capsys.readouterr()
    assert code == 0
    # Stdout: exactly one JSON object, the machine contract untouched.
    assert cap.out.count("\n") == 1
    payload = _json.loads(cap.out)
    assert set(payload) >= {
        "pr", "head", "verdict", "settled", "green", "pr_state", "mergeable",
        "checks", "optional_reviews", "optional_reviews_unresolved",
        "review_coverage", "review_activity", "dispatch_hold", "ready",
        "ready_blockers",
    }
    # Stderr: the human line first, rendered from exactly that payload (the
    # coverage note may follow it). The clean line itself is pinned by the
    # pure test above; this one pins that the two streams never trade.
    assert cap.err.split("\n", 1)[0] == _status.verdict_line(payload)


def test_run_status_emits_json_and_code(monkeypatch, capsys):
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
            "headRefOid": "h" * 40,
        }, ""),
    )
    # Stub the review read (no gh) so the frozen contract is deterministic.
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {
            "optional_reviews": [],
            "optional_reviews_unresolved": 0,
            "optional_reviews_resolved_unchanged": 0,
        },
    )
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 2},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    code = _status.run_status("42")
    assert code == 0
    import json

    out = json.loads(capsys.readouterr().out)
    assert out == {
        "pr": "42",
        "head": "h" * 40,
        "verdict": "green",
        "settled": True,
        "green": True,
        "pr_state": "OPEN",
        "mergeable": None,
        "checks": {
            "total": 1,
            "check_runs": 1,
            "statuses": 0,
            "fail_check_runs": 0,
            "fail_statuses": 0,
            "pass": 1,
            "fail": 0,
            "pending": 0,
            "unsettled": 0,
            "unsettled_fail": 0,
        },
        "optional_reviews": [],
        "optional_reviews_unresolved": 0,
        "optional_reviews_resolved_unchanged": 0,
        "review_coverage": {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 2},
        "review_activity": {
            "blocker": "",
            "detail": "",
            "hold": None,
            "worktree": {
                "probed": True,
                "path": None,
                "dirty": None,
                "head": None,
                "note": "stubbed",
            },
        },
        "dispatch_hold": None,
        "ready": True,
        "ready_blockers": [],
    }


def test_dispatch_hold_removes_green_pr_from_ready_set(monkeypatch, capsys):
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "headRefOid": "abc",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 2, "head_sha": "abc"},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    monkeypatch.setattr(
        _status,
        "_merge_hold_reason",
        lambda pr, cwd: "dispatch-hold:x-5a5c: blocking; set_by=king",
    )
    assert _status.run_status("42") == 0
    out = _json.loads(capsys.readouterr().out)
    assert out["ready"] is False
    assert out["ready_blockers"] == ["dispatch_hold"]
    assert "set_by=king" in out["dispatch_hold"]


# ---- x-e601: ready conjoins review coverage, ready_blockers names the conjunct ----
#
# `fno do pr merge` already refused on uncovered coverage while this verb printed
# ready: true from the same payload (the specimen set: five PRs at once, each
# green and uncovered). ready now conjoins coverage exactly the way merge
# reads it, and the blockers list is the positive marker for WHICH conjunct
# failed - a bare false has one explanation per conjunct.


def _green_fetch(monkeypatch):
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    # A lane is on by default so the coverage conjunct is the one under test,
    # never the machine's own config (the same predicate merge reads).
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    # The local code-review requirement is off by default so a test isolates
    # one conjunct at a time; the conjunct's own tests enable it explicitly.
    monkeypatch.setattr(
        "fno.pr._merge._code_review_attestation_required",
        lambda repo, pr_number=0: False,
    )


def test_ready_is_false_when_coverage_is_uncovered(monkeypatch, capsys):
    """AC3-HP: green CI and zero unresolved findings no longer say ready while
    the merge gate refuses the same PR for zero coverage."""
    import json

    _green_fetch(monkeypatch)
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "uncovered", "reviewed_count": 0},
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["green"] is True
    assert out["ready"] is False
    assert "review_coverage_uncovered" in out["ready_blockers"]


def test_ready_names_reviewer_refused_and_the_reviewer(monkeypatch, capsys):
    import json

    _green_fetch(monkeypatch)
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {
            "coverage": "uncovered",
            "review_state": "reviewer_refused",
            "reviewed_count": 0,
            "verdicts": [
                {
                    "producer": "github_app",
                    "name": "chatgpt-codex-connector",
                    "verdict": "refused",
                }
            ],
        },
    )
    _status.run_status("42")
    captured = capsys.readouterr()
    out = json.loads(captured.out)
    assert out["ready"] is False
    assert "review_coverage_reviewer_refused" in out["ready_blockers"]
    assert "chatgpt-codex-connector" in captured.err
    assert "reviewer_refused" in captured.err


def _coverage_status_projection_fetch(monkeypatch, posted_state="SUCCESS", *, state="OPEN"):
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: (
            {
                "state": state,
                "headRefOid": "1" * 40,
                "statusCheckRollup": [
                    {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    {
                        "context": _reviews.COVERAGE_STATUS_CONTEXT,
                        "state": posted_state,
                    },
                ],
            },
            "",
        ),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    monkeypatch.setattr(
        "fno.pr._merge._code_review_attestation_required",
        lambda repo, pr_number=0: False,
    )


def test_status_reposts_a_stale_green_when_computed_coverage_is_uncovered(
    monkeypatch, capsys
):
    _coverage_status_projection_fetch(monkeypatch)
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "uncovered", "reviewed_count": 0},
    )
    calls = []
    monkeypatch.setattr(
        _reviews,
        "publish_coverage_status",
        lambda pr, **kw: (calls.append((pr, kw)) is None, ""),
    )

    code = _status.run_status("42")
    out = _json.loads(capsys.readouterr().out)

    assert code == 0
    assert calls == [(42, {"head": "1" * 40, "cwd": None})]
    assert out["coverage_status_repost"] == "reposted"
    assert out["ready"] is False
    assert "review_coverage_uncovered" in out["ready_blockers"]


@pytest.mark.parametrize(
    "posted_state, coverage",
    [
        (
            "SUCCESS",
            {
                "coverage": "covered",
                "review_state": "reviewed",
                "reviewed_count": 1,
                "head_sha": "1" * 40,
            },
        ),
        (
            "FAILURE",
            {
                "coverage": "uncovered",
                "review_state": "unreviewed",
                "reviewed_count": 0,
            },
        ),
    ],
)
def test_status_does_not_repost_when_the_posted_state_agrees(
    monkeypatch, capsys, posted_state, coverage
):
    _coverage_status_projection_fetch(monkeypatch, posted_state)
    monkeypatch.setattr(_status, "read_review_coverage", lambda pr, cwd, **kw: coverage)
    monkeypatch.setattr(
        _reviews,
        "publish_coverage_status",
        lambda *_a, **_kw: pytest.fail("an agreeing status was reposted"),
    )

    _status.run_status("42")
    out = _json.loads(capsys.readouterr().out)

    assert "coverage_status_repost" not in out


def test_status_does_not_become_the_first_coverage_status_writer(monkeypatch, capsys):
    _green_fetch(monkeypatch)
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "uncovered", "reviewed_count": 0},
    )
    monkeypatch.setattr(
        _reviews,
        "publish_coverage_status",
        lambda *_a, **_kw: pytest.fail("an absent context triggered a first post"),
    )

    _status.run_status("42")
    out = _json.loads(capsys.readouterr().out)

    assert "coverage_status_repost" not in out


def test_status_reports_a_failed_repost_without_changing_its_verdict(
    monkeypatch, capsys
):
    _coverage_status_projection_fetch(monkeypatch)
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "uncovered", "reviewed_count": 0},
    )
    monkeypatch.setattr(
        _reviews,
        "publish_coverage_status",
        lambda *_a, **_kw: (False, "gh exited 1"),
    )

    code = _status.run_status("42")
    out = _json.loads(capsys.readouterr().out)

    assert code == 0
    assert out["verdict"] == "green"
    assert out["ready"] is False
    assert out["ready_blockers"] == ["review_coverage_uncovered"]
    assert out["coverage_status_repost"] == "repost failed: gh exited 1"


def test_status_never_reposts_coverage_for_a_terminal_pr(monkeypatch, capsys):
    _coverage_status_projection_fetch(monkeypatch, state="MERGED")
    monkeypatch.setattr(
        _reviews,
        "publish_coverage_status",
        lambda *_a, **_kw: pytest.fail("a terminal PR triggered a coverage post"),
    )

    _status.run_status("42")
    out = _json.loads(capsys.readouterr().out)

    assert "coverage_status_repost" not in out


def test_ready_is_false_when_coverage_is_unknown(monkeypatch, capsys):
    """AC3-EDGE: an unknown coverage read blocks and is named as its own
    blocker. WHY the read degraded is x-b56a's question; this only reports
    that the answer is missing."""
    import json

    _green_fetch(monkeypatch)
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "unknown", "reviewed_count": None},
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is False
    assert "review_coverage_unknown" in out["ready_blockers"]
    assert "review_coverage_uncovered" not in out["ready_blockers"]


def test_ready_treats_a_legacy_covered_zero_as_uncovered(monkeypatch, capsys):
    """Historical events serialize a real zero as `covered` with count 0;
    without validated verdict state, ready still fails closed."""
    import json

    _green_fetch(monkeypatch)
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "covered", "reviewed_count": 0},
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is False
    assert "review_coverage_uncovered" in out["ready_blockers"]


def test_ready_blockers_name_the_ci_conjunct_too(monkeypatch, capsys):
    """The list is not coverage-only: a red verdict is named with the verdict's
    own word, so a reader never guesses which of the conjuncts failed."""
    import json

    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 2},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    monkeypatch.setattr(
        "fno.pr._merge._code_review_attestation_required",
        lambda repo, pr_number=0: False,
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is False
    assert out["ready_blockers"] == ["ci_red"]


@pytest.mark.parametrize(
    "mergeable, expected_ready, expected_blocker",
    [
        pytest.param(
            "CONFLICTING", False, "not_mergeable_conflicting", id="conflicting"
        ),
        pytest.param(
            "UNKNOWN", False, "not_mergeable_unknown",
            id="still-computing-a-null-from-github-maps-to-the-string-unknown",
        ),
        pytest.param("MERGEABLE", True, None, id="the-common-case-adds-no-blocker"),
    ],
)
def test_ready_reflects_mergeable(
    monkeypatch, capsys, mergeable, expected_ready, expected_blocker
):
    """x-4271: PR 965 read verdict green / ready true / ready_blockers empty
    at mergeable=CONFLICTING, because `mergeable` never reached the ready
    conjunction. A dirty or still-computing merge state must block ready
    regardless of what the (self-published, real-CI-free) check tally says."""
    import json

    _green_fetch(monkeypatch)
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
            "mergeable": mergeable,
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 2},
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is expected_ready
    if expected_blocker is None:
        assert out["ready_blockers"] == []
    else:
        assert expected_blocker in out["ready_blockers"]


def test_ready_skips_the_coverage_conjunct_on_a_no_lane_repo(monkeypatch, capsys):
    """A stock install with no review lane opted OUT of the coverage guard on
    the merge side (the x-0eaf boundary: `_review_lane_configured` gates the
    merge guard); ready must opt out with it, or the two verbs answer opposite
    ways again - now status refusing forever at uncovered 0 on every green PR
    of a repo merge would merge."""
    import json

    _green_fetch(monkeypatch)
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: False)
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "uncovered", "reviewed_count": 0},
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is True
    assert out["ready_blockers"] == []


def _lane_fetch(monkeypatch, *, state="OPEN", head="h1"):
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: (
            {
                "state": state,
                "headRefOid": head,
                "statusCheckRollup": [
                    {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}
                ],
            },
            "",
        ),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    monkeypatch.setattr(
        "fno.pr._merge._code_review_attestation_required",
        lambda repo, pr_number=0: False,
    )


def test_ready_requires_the_local_code_review_pass_merge_does(monkeypatch, capsys):
    """PR 917 dual review: with the lane requiring the harness review verb,
    ready must not pass on a bot-only pass - merge refuses that row, so status
    answering ready is the two-readers-disagree shape again."""
    import json

    _lane_fetch(monkeypatch)
    monkeypatch.setattr(
        "fno.pr._merge._code_review_attestation_required",
        lambda repo, pr_number=0: True,
    )
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {
            "coverage": "covered",
            "review_state": "reviewed",
            "reviewed_count": 1,
            "verdicts": [
                {
                    "name": "chatgpt-codex-connector",
                    "producer": "github_app",
                    "verdict": "reviewed",
                }
            ],
        },
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is False
    assert "review_coverage_no_local_pass" in out["ready_blockers"]


def test_ready_passes_with_the_local_code_review_pass_present(monkeypatch, capsys):
    import json

    _lane_fetch(monkeypatch)
    monkeypatch.setattr(
        "fno.pr._merge._code_review_attestation_required",
        lambda repo, pr_number=0: True,
    )
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {
            "coverage": "covered",
            "review_state": "reviewed",
            "reviewed_count": 1,
            "head_sha": "h1",
            "verdicts": [
                {
                    "name": "code-review",
                    "producer": "local_attestation",
                    "verdict": "reviewed",
                }
            ],
        },
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is True
    assert out["ready_blockers"] == []


def test_ready_exempts_a_merged_pr_from_the_coverage_conjunct(monkeypatch, capsys):
    """PR 917 dual review: the gate guards what WOULD merge; a PR merged
    out-of-band has no would left, so an uncovered row on it is history, not
    a blocker."""
    import json

    _lane_fetch(monkeypatch, state="MERGED")
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "uncovered", "reviewed_count": 0},
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is True
    assert out["ready_blockers"] == []
    # And it says WHY it has no coverage number, in a word that is not the
    # instrument-failed sentinel. `unknown` here read as "the probe died" on
    # every merged PR - including the path king-for-a-day now prescribes - and
    # carries its own `review_coverage_unknown` blocker. A deliberate skip and
    # a broken probe must never share a spelling.
    assert out["review_coverage"]["coverage"] == "not_asked"
    assert out["review_coverage"]["reviewed_count"] == 0


def test_a_closed_pr_also_reports_not_asked_rather_than_unknown(monkeypatch, capsys):
    """CLOSED takes the same terminal arm as MERGED, so it must report the
    same deliberate skip. Asserted separately because the arm tests one state
    and branches on two, and only MERGED had a test."""
    import json

    _lane_fetch(monkeypatch, state="CLOSED")
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["review_coverage"]["coverage"] == "not_asked"
    assert "review_coverage_unknown" not in out["ready_blockers"]


def test_ready_names_a_stale_head_pin(monkeypatch, capsys):
    """PR 917 dual review: a covered row pinned to an older head is not ready;
    merge compares the pin, so status reading it as covered disagrees."""
    import json

    _lane_fetch(monkeypatch, head="h2")
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {
            "coverage": "covered",
            "review_state": "reviewed",
            "reviewed_count": 1,
            "head_sha": "h1",
        },
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is False
    assert "review_coverage_stale_head" in out["ready_blockers"]


def test_ready_does_not_crash_on_a_non_integer_reviewed_count(monkeypatch, capsys):
    """PR 917 dual review: a malformed count coerces through the same
    _safe_int merge reads instead of raising TypeError out of the verb."""
    import json

    _lane_fetch(monkeypatch)
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "covered", "review_state": "reviewed", "reviewed_count": "2"},
    )
    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is True
    assert out["ready_blockers"] == []


def test_local_pass_conjunct_is_satisfiable_on_the_real_read_path(
    monkeypatch, capsys, tmp_path
):
    """Round 3, PR 917: read_review_coverage's shaped row dropped `verdicts`,
    so the local-pass conjunct saw an empty list forever on this repo's config
    while merge, reading the raw row, accepted - the two-readers-disagree shape.
    This drives run_status through the REAL reader (only the repo root is
    pointed at the fixture), so the conjunct is proven on the wire, not on the
    stubbed shapes the other tests pin."""
    import json

    _lane_fetch(monkeypatch)
    monkeypatch.setattr(
        "fno.pr._merge._code_review_attestation_required",
        lambda repo, pr_number=0: True,
    )
    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True)
    covered_row = {
        "ts": "2026-08-17T00:00:00Z",
        "type": "review_coverage",
        "data": {
            "pr": 42,
            "coverage": "covered",
            "review_state": "reviewed",
            "reviewed_count": 1,
            "head_sha": "h1",
            "verdicts": [
                {
                    "name": "code-review",
                    "producer": "local_attestation",
                    "verdict": "reviewed",
                    "reviewed_sha": "h1",
                    "freshness": "fresh",
                }
            ],
        },
    }
    events.write_text(json.dumps(covered_row) + "\n", encoding="utf-8")
    monkeypatch.setattr(_reviews, "_repo_root", lambda cwd=None: tmp_path)
    monkeypatch.setattr(
        _reviews, "_reviewed_sha_is_ancestor", lambda reviewed, head, cwd: True
    )
    _status.run_status("42", cwd=str(tmp_path))
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is True, out["ready_blockers"]
    assert out["review_coverage"]["verdicts"][0]["name"] == "code-review"

    # Same wire, no local pass in the verdicts: the conjunct fails BY NAME, so
    # the negative direction is also proven on the reader, not the stub.
    bot_only = json.loads(json.dumps(covered_row))
    bot_only["data"]["verdicts"] = [
        {"name": "chatgpt-codex-connector", "producer": "github_app",
         "verdict": "reviewed", "reviewed_sha": "h1", "freshness": "fresh"}
    ]
    events.write_text(json.dumps(bot_only) + "\n", encoding="utf-8")
    _status.run_status("42", cwd=str(tmp_path))
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is False
    assert "review_coverage_no_local_pass" in out["ready_blockers"]


def test_closed_pr_skips_the_probes_and_the_coverage_conjunct(monkeypatch, capsys):
    """Round 3, PR 917: a terminal PR (CLOSED here, MERGED already exempt) has
    no would-merge left, yet the optional-review read, the lane probes, and the
    coverage recompute still fired against it - live reads a closed PR burns
    for a conjunct that guards nothing. Every probe here RAISES: reaching any
    of them fails the test, which is the assertion."""
    import json

    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: (
            {
                "state": "CLOSED",
                "headRefOid": "h1",
                "statusCheckRollup": [
                    {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}
                ],
            },
            "",
        ),
    )

    def _must_not_run(*_a, **_k):
        raise AssertionError("a probe fired on a closed PR")

    monkeypatch.setattr(_status, "read_optional_review_state", _must_not_run)
    monkeypatch.setattr(_status, "read_review_coverage", _must_not_run)
    monkeypatch.setattr(_status, "_review_lane", _must_not_run)
    monkeypatch.setattr(
        "fno.pr._merge._code_review_attestation_required", _must_not_run
    )
    code = _status.run_status("42")
    assert code == 0
    out = json.loads(capsys.readouterr().out)
    assert out["ready"] is True
    assert out["ready_blockers"] == []
    # The no-pending answers, not `unknown`: nothing failed, it was not asked.
    assert out["optional_reviews"] == []
    assert out["optional_reviews_unresolved"] == 0
    assert out["optional_reviews_resolved_unchanged"] == 0


def test_read_review_coverage_from_events(tmp_path):
    """x-0eaf: read_review_coverage consumes the latest review_coverage event
    for the PR from the project events log; missing verdict proof fails closed."""
    import json

    from fno.pr._reviews import read_review_coverage

    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True)
    events.write_text(
        json.dumps(
            {"type": "review_coverage", "data": {"pr": 7, "coverage": "covered", "review_state": "reviewed", "reviewed_count": 1, "head_sha": "a"}}
        )
        + "\n",
        encoding="utf-8",
    )
    assert read_review_coverage(7, cwd=str(tmp_path)) == {
        "coverage": "uncovered",
        "review_state": "unreviewed",
        "reviewed_count": 0,
        "self_attested_count": None,
        "head_sha": "a",
        "stale_verdicts": [],
        "verdicts": [],
    }
    # A different PR -> no event -> unknown sentinel.
    assert read_review_coverage(99, cwd=str(tmp_path)) == {
        "coverage": "unknown",
        "reviewed_count": None,
        "self_attested_count": None,
        "head_sha": None,
        "stale_verdicts": [],
    }


def test_shape_review_coverage_derives_state_from_validated_verdicts():
    cases = [
        (
            {
                "coverage": "covered",
                "verdicts": [
                    {
                        "producer": "local_attestation",
                        "name": "code-review",
                        "verdict": "reviewed",
                        "reviewed_sha": "abc",
                        "freshness": "fresh",
                    },
                    {
                        "producer": "github_app",
                        "name": "chatgpt-codex-connector",
                        "verdict": "refused",
                    },
                ],
            },
            "reviewed",
        ),
        (
            {
                "coverage": "uncovered",
                "verdicts": [
                    {
                        "producer": "github_app",
                        "name": "chatgpt-codex-connector",
                        "verdict": "refused",
                    }
                ],
            },
            "reviewer_refused",
        ),
        (
            {
                "coverage": "uncovered",
                "verdicts": [
                    {
                        "producer": "github_app",
                        "name": "chatgpt-codex-connector",
                        "verdict": "absent",
                    }
                ],
            },
            "unreviewed",
        ),
    ]
    for event, expected in cases:
        shaped = _reviews._shape_review_coverage(event, None, None)
        assert shaped["review_state"] == expected

    unknown = _reviews._shape_review_coverage(
        {"coverage": "unknown", "review_state": "unreviewed", "verdicts": []},
        None,
        None,
    )
    assert "review_state" not in unknown


def test_read_review_coverage_surfaces_stale_verdicts(tmp_path):
    """x-5b99: a stale verdict and a fresh one used to render identically.

    The specimen is PR #826: codex reviewed 8e557ccd while the gate evaluated
    against 89bc0b91, and status printed "covered, reviewed_count 2" with no
    way to learn that one of those two verdicts was for a commit codex never
    saw.
    """
    import json

    from fno.pr._reviews import read_review_coverage

    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True)
    events.write_text(
        json.dumps(
            {
                "type": "review_coverage",
                "data": {
                    "pr": 826,
                    "coverage": "covered",
                    "review_state": "reviewed",
                    "reviewed_count": 1,
                    "self_attested_count": 1,
                    "head_sha": "89bc0b91",
                    "verdicts": [
                        {
                            "producer": "github_app",
                            "name": "chatgpt-codex-connector",
                            "verdict": "stale",
                            "reviewed_sha": "8e557ccd",
                            "freshness": "stale",
                        },
                        {
                            "producer": "local_attestation",
                            "name": "code-review",
                            "verdict": "reviewed",
                            "reviewed_sha": "89bc0b91",
                            "freshness": "fresh",
                        },
                    ],
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )
    got = read_review_coverage(826, cwd=str(tmp_path))
    assert got["coverage"] == "uncovered"
    assert got["head_sha"] == "89bc0b91"
    assert got["self_attested_count"] == 1
    assert got["stale_verdicts"] == [
        {
            "name": "chatgpt-codex-connector",
            "producer": "github_app",
            "reviewed_sha": "8e557ccd",
            "freshness": "stale",
        }
    ]


def test_refused_verdict_without_freshness_is_not_reported_stale(tmp_path):
    from fno.pr._reviews import read_review_coverage

    head = _commit_reviewed_history(tmp_path)
    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True)
    events.write_text(
        _json.dumps(
            {
                "type": "review_coverage",
                "data": {
                    "pr": 1006,
                    "coverage": "covered",
                    "review_state": "reviewed",
                    "reviewed_count": 1,
                    "head_sha": head,
                    "verdicts": [
                        {
                            "producer": "local_attestation",
                            "name": "code-review",
                            "verdict": "reviewed",
                            "reviewed_sha": head,
                            "freshness": "fresh",
                        },
                        {
                            "producer": "github_app",
                            "name": "optional-reviewer",
                            "verdict": "refused",
                            "freshness": "stale",
                        },
                    ],
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )

    got = read_review_coverage(1006, cwd=str(tmp_path), head=head)
    assert got["coverage"] == "covered"
    assert got["stale_verdicts"] == []
    assert "freshness" not in got["verdicts"][1]


@pytest.mark.parametrize(
    "malformed_kind", ["non-dict", "missing-identity", "unknown-producer"]
)
def test_malformed_verdict_cannot_hide_beside_fresh_review(
    tmp_path, malformed_kind
):
    from fno.pr._reviews import read_review_coverage

    head = _commit_reviewed_history(tmp_path)
    malformed = {
        "non-dict": "malformed",
        "missing-identity": {
            "verdict": "reviewed",
            "reviewed_sha": head,
            "freshness": "fresh",
        },
        "unknown-producer": {
            "producer": "not-a-producer",
            "name": "invented-reviewer",
            "verdict": "reviewed",
            "reviewed_sha": head,
            "freshness": "fresh",
        },
    }[malformed_kind]
    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True)
    events.write_text(
        _json.dumps(
            {
                "type": "review_coverage",
                "data": {
                    "pr": 1007,
                    "coverage": "covered",
                    "review_state": "reviewed",
                    "reviewed_count": 1,
                    "head_sha": head,
                    "verdicts": [
                        {
                            "producer": "local_attestation",
                            "name": "code-review",
                            "verdict": "reviewed",
                            "reviewed_sha": head,
                            "freshness": "fresh",
                        },
                        malformed,
                    ],
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )

    got = read_review_coverage(1007, cwd=str(tmp_path), head=head)
    assert got["coverage"] == "uncovered"


def _git(tmp_path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=tmp_path,
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def _review_coverage_event(tmp_path, pr: int, head: str, verdict: dict) -> None:
    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True)
    events.write_text(
        _json.dumps(
            {
                "type": "review_coverage",
                "data": {
                    "pr": pr,
                    "coverage": "covered",
                    "review_state": "reviewed",
                    "reviewed_count": 1,
                    "head_sha": head,
                    "verdicts": [verdict],
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )


def _commit_reviewed_history(tmp_path) -> str:
    _git(tmp_path, "init", "-q")
    _git(tmp_path, "config", "user.email", "review-test@example.com")
    _git(tmp_path, "config", "user.name", "Review Test")
    (tmp_path / "reviewed.txt").write_text("reviewed\n", encoding="utf-8")
    _git(tmp_path, "add", "reviewed.txt")
    _git(tmp_path, "commit", "-qm", "reviewed history")
    return _git(tmp_path, "rev-parse", "HEAD")


def test_read_review_coverage_rejects_rebased_out_review(
    tmp_path, monkeypatch, capsys
):
    """A stored fresh stamp cannot survive when its commit leaves HEAD history."""
    from fno.pr._reviews import read_review_coverage

    reviewed_sha = _commit_reviewed_history(tmp_path)
    _git(tmp_path, "checkout", "--orphan", "rewritten")
    (tmp_path / "rewritten.txt").write_text("rewritten\n", encoding="utf-8")
    _git(tmp_path, "add", "rewritten.txt")
    _git(tmp_path, "commit", "-qm", "rewritten history")
    head = _git(tmp_path, "rev-parse", "HEAD")
    _review_coverage_event(
        tmp_path,
        1003,
        head,
        {
            "producer": "local_attestation",
            "name": "code-review",
            "verdict": "reviewed",
            "reviewed_sha": reviewed_sha,
            "freshness": "fresh",
        },
    )

    got = read_review_coverage(1003, cwd=str(tmp_path), head=head)
    assert got["coverage"] != "covered"
    assert got["verdicts"][0]["freshness"] == "stale"
    assert got["stale_verdicts"] == [
        {
            "name": "code-review",
            "producer": "local_attestation",
            "reviewed_sha": reviewed_sha,
            "freshness": "stale",
        }
    ]

    _lane_fetch(monkeypatch, head=head)
    monkeypatch.setattr(_reviews, "_repo_root", lambda cwd=None: tmp_path)
    _status.run_status("1003", cwd=str(tmp_path))
    status = _json.loads(capsys.readouterr().out)
    assert status["ready"] is False
    assert "review_coverage_uncovered" in status["ready_blockers"]


def test_read_review_coverage_rejects_verdict_without_freshness(tmp_path):
    """Missing freshness metadata is unknown evidence, never a fresh review."""
    from fno.pr._reviews import read_review_coverage

    head = _commit_reviewed_history(tmp_path)
    _review_coverage_event(
        tmp_path,
        1004,
        head,
        {
            "producer": "local_attestation",
            "name": "code-review",
            "verdict": "reviewed",
            "reviewed_sha": head,
        },
    )

    got = read_review_coverage(1004, cwd=str(tmp_path), head=head)
    assert got["coverage"] != "covered"
    assert got["verdicts"][0]["freshness"] == "stale"
    assert got["stale_verdicts"][0]["reviewed_sha"] == head


def test_read_review_coverage_preserves_ancestor_review(tmp_path):
    from fno.pr._reviews import read_review_coverage

    reviewed_sha = _commit_reviewed_history(tmp_path)
    (tmp_path / "later.txt").write_text("later\n", encoding="utf-8")
    _git(tmp_path, "add", "later.txt")
    _git(tmp_path, "commit", "-qm", "later history")
    head = _git(tmp_path, "rev-parse", "HEAD")
    _review_coverage_event(
        tmp_path,
        1005,
        head,
        {
            "producer": "local_attestation",
            "name": "code-review",
            "verdict": "reviewed",
            "reviewed_sha": reviewed_sha,
            "freshness": "fresh",
        },
    )

    got = read_review_coverage(1005, cwd=str(tmp_path), head=head)
    assert got["coverage"] == "covered"
    assert got["verdicts"][0]["freshness"] == "fresh"
    assert got["stale_verdicts"] == []


def test_error_verdict_carries_the_reason(monkeypatch, capsys):
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: (None, "quota gone"))
    assert _status.run_status("99") == 4
    assert _json.loads(capsys.readouterr().out)["reason"] == "quota gone"


def test_run_status_fetch_failure_is_error(monkeypatch, capsys):
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: (None, "boom"))
    code = _status.run_status("99")
    assert code == 4
    import json

    out = json.loads(capsys.readouterr().out)
    assert out["verdict"] == "error"
    assert out["settled"] is False


# --- x-705b: optional-review signal on `fno do pr status` ---------------------
#
# These exercise read_optional_review_state with a fake `runner` (no gh) plus the
# run_status integration. "Optional" resolves to the hardcoded bots regardless of
# config (config read is best-effort and wrapped), so gemini/codex are stable.

_URL = "https://github.com/o/r/pull/42"


def _fake_runner(*, reviews, threads, view_ok=True, graphql_ok=True):
    """Dispatch gh calls: `pr view` -> reviews+url, `api graphql` -> threads."""

    def runner(cmd, *, cwd=None, timeout=None, **_):
        if "rate_limit" in cmd:
            return Result(0, _json.dumps(
                {"resources": {"graphql": {"remaining": 5000, "reset": 1787072400}}}
            ), "")
        if "graphql" in cmd:
            if not graphql_ok:
                return Result(1, "", "boom")
            return Result(0, _json.dumps(threads), "")
        # gh pr view ... --json reviews,url
        if not view_ok:
            return Result(1, "", "boom")
        return Result(0, _json.dumps({"url": _URL, "reviews": reviews}), "")

    return runner


def _threads_payload(nodes):
    return {"data": {"repository": {"pullRequest": {"reviewThreads": {
        "pageInfo": {"hasNextPage": False, "endCursor": None},
        "nodes": nodes,
    }}}}}


def _thread(login, resolved, outdated):
    return {
        "isResolved": resolved,
        "isOutdated": outdated,
        "comments": {"nodes": [{"author": {"login": login}}]},
    }


def test_ac1_hp_unresolved_optional_findings_surface():
    """AC1-HP: green PR + gemini COMMENTED with 2 unresolved threads."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(
            reviews=[{"author": {"login": "gemini-code-assist[bot]"}, "state": "COMMENTED"}],
            threads=_threads_payload([
                _thread("gemini-code-assist[bot]", False, False),
                _thread("gemini-code-assist[bot]", False, False),
            ]),
        ),
    )
    assert state["optional_reviews_unresolved"] == 2
    assert state["optional_reviews"] == [
        {"author": "gemini-code-assist", "state": "COMMENTED", "inline_count": 2}
    ]


def test_ac1_edge_resolving_drops_count_to_zero():
    """AC1-EDGE/US5: all optional threads resolved -> 0, bot still listed."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(
            reviews=[{"author": {"login": "gemini-code-assist[bot]"}, "state": "COMMENTED"}],
            threads=_threads_payload([
                _thread("gemini-code-assist[bot]", True, True),
                _thread("gemini-code-assist[bot]", True, True),
            ]),
        ),
    )
    assert state["optional_reviews_unresolved"] == 0
    assert state["optional_reviews"][0]["author"] == "gemini-code-assist"


def test_ac1_hp_resolved_unchanged_optional_thread_is_counted():
    """AC1-HP: resolved + current line is visible as a separate fact."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(
            reviews=[{"author": {"login": "gemini-code-assist[bot]"}, "state": "COMMENTED"}],
            threads=_threads_payload([
                _thread("gemini-code-assist[bot]", True, False),
            ]),
        ),
    )
    assert state["optional_reviews_unresolved"] == 0
    assert state["optional_reviews_resolved_unchanged"] == 1


def test_ac1_edge_counts_fixed_unresolved_and_resolved_unchanged_threads():
    """AC1-EDGE: the two booleans classify each optional thread independently."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(
            reviews=[{"author": {"login": "gemini-code-assist[bot]"}, "state": "COMMENTED"}],
            threads=_threads_payload([
                _thread("gemini-code-assist[bot]", True, True),
                _thread("gemini-code-assist[bot]", False, False),
                _thread("gemini-code-assist[bot]", True, False),
            ]),
        ),
    )
    assert state["optional_reviews_unresolved"] == 1
    assert state["optional_reviews_resolved_unchanged"] == 1


def test_ac1_fr_non_optional_review_excluded():
    """AC1-FR/US3: an arbitrary human login is neither listed nor counted."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(
            reviews=[{"author": {"login": "some-human"}, "state": "CHANGES_REQUESTED"}],
            threads=_threads_payload([_thread("some-human", False, False)]),
        ),
    )
    assert state["optional_reviews"] == []
    assert state["optional_reviews_unresolved"] == 0


def _fake_runner_with_quota(*, remaining, reviews=None, threads=None):
    """Like _fake_runner, but also answers `gh api rate_limit` and counts
    every `gh pr view` and `gh api graphql` invocation - both spend the same
    shared GraphQL quota (`_status.py`'s `_fetch` docstring), so the floor
    must prevent both, not just the reviewThreads call."""
    calls = {"graphql": 0, "view": 0}

    def runner(cmd, *, cwd=None, timeout=None, **_):
        if "rate_limit" in cmd:
            return Result(0, _json.dumps(
                {"resources": {"graphql": {"remaining": remaining, "reset": 1787072400}}}
            ), "")
        if "graphql" in cmd:
            calls["graphql"] += 1
            return Result(0, _json.dumps(threads or _threads_payload([])), "")
        calls["view"] += 1
        return Result(0, _json.dumps({"url": _URL, "reviews": reviews or []}), "")

    return runner, calls


def test_below_floor_returns_unknown_and_spends_no_query():
    runner, calls = _fake_runner_with_quota(remaining=50)
    state = _reviews.read_optional_review_state("42", runner=runner)
    assert state == dict(_reviews._UNKNOWN)
    assert calls["graphql"] == 0, "the floor must trip before the reviewThreads query fires"
    assert calls["view"] == 0, "the floor must trip before `gh pr view` fires too"


def test_above_floor_reads_threads_normally():
    runner, calls = _fake_runner_with_quota(
        remaining=5000,
        reviews=[{"author": {"login": "gemini-code-assist[bot]"}, "state": "COMMENTED"}],
        threads=_threads_payload([_thread("gemini-code-assist[bot]", False, False)]),
    )
    state = _reviews.read_optional_review_state("42", runner=runner)
    assert state["optional_reviews_unresolved"] == 1
    assert calls["graphql"] == 1
    assert calls["view"] == 1


def test_body_only_commented_review_lists_with_zero_inline():
    """Boundary: a body-only COMMENTED review (no thread) still lists via reviews."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(
            reviews=[{"author": {"login": "chatgpt-codex-connector[bot]"}, "state": "COMMENTED"}],
            threads=_threads_payload([]),
        ),
    )
    assert state["optional_reviews"] == [
        {"author": "chatgpt-codex-connector", "state": "COMMENTED", "inline_count": 0}
    ]
    assert state["optional_reviews_unresolved"] == 0


def test_none_posted_is_empty_list_not_unknown():
    """AC1-UI: no optional reviews -> [] / 0, distinct from the unknown sentinel."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(reviews=[], threads=_threads_payload([])),
    )
    assert state["optional_reviews"] == []
    assert state["optional_reviews_unresolved"] == 0


def test_ac1_err_view_failure_degrades_to_unknown():
    """AC1-ERR/US4: a failed review read -> unknown / None sentinels."""
    state = _reviews.read_optional_review_state(
        "42", runner=_fake_runner(reviews=[], threads={}, view_ok=False)
    )
    assert state == dict(_reviews._UNKNOWN)


def test_graphql_failure_degrades_to_unknown():
    """AC1-ERR/US4: a failed thread read (gh graphql error) -> unknown / None."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(
            reviews=[{"author": {"login": "gemini-code-assist[bot]"}, "state": "COMMENTED"}],
            threads={},
            graphql_ok=False,
        ),
    )
    assert state == dict(_reviews._UNKNOWN)


def test_graphql_errors_envelope_degrades_to_unknown():
    """A GraphQL error envelope (rc=0, `errors` set) is unavailable, not empty."""
    def runner(cmd, *, cwd=None, timeout=None, **_):
        if "rate_limit" in cmd:
            return Result(0, _json.dumps(
                {"resources": {"graphql": {"remaining": 5000, "reset": 1787072400}}}
            ), "")
        if "graphql" in cmd:
            return Result(0, _json.dumps({"errors": [{"message": "nope"}]}), "")
        return Result(0, _json.dumps({"url": _URL, "reviews": []}), "")

    state = _reviews.read_optional_review_state("42", runner=runner)
    assert state == dict(_reviews._UNKNOWN)


def test_missing_is_outdated_degrades_to_unknown():
    """AC3-ERR: missing discriminator evidence is unavailable, never false."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(
            reviews=[{"author": {"login": "gemini-code-assist[bot]"}, "state": "COMMENTED"}],
            threads=_threads_payload([
                {
                    "isResolved": True,
                    "comments": {"nodes": [{"author": {"login": "gemini-code-assist[bot]"}}]},
                },
            ]),
        ),
    )
    assert state == dict(_reviews._UNKNOWN)


def test_non_object_json_degrades_to_unknown():
    """US4: a valid-but-non-object JSON body degrades, never AttributeErrors."""
    def runner(cmd, *, cwd=None, timeout=None, **_):
        return Result(0, _json.dumps(["not", "an", "object"]), "")

    state = _reviews.read_optional_review_state("42", runner=runner)
    assert state == dict(_reviews._UNKNOWN)


def test_us2_green_with_unresolved_optional_still_exits_zero(monkeypatch, capsys):
    """US2/AC1-UI: an unresolved optional finding never changes the exit code."""
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {
            "optional_reviews": [{"author": "gemini-code-assist", "state": "COMMENTED", "inline_count": 2}],
            "optional_reviews_unresolved": 2,
        },
    )
    # Coverage stubbed to a counted pass so the ONLY blocker under test is the
    # unresolved optional finding (ready conjoins coverage since x-e601).
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 2},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    code = _status.run_status("42")
    assert code == 0  # green exit unchanged despite an unresolved optional finding
    out = _json.loads(capsys.readouterr().out)
    assert out["green"] is True
    assert out["optional_reviews_unresolved"] == 2
    assert out["ready"] is False  # green but not ready: the actionable signal


def test_status_surfaces_resolved_unchanged_as_advisory(monkeypatch, capsys):
    """AC2-HP: the count is visible without changing readiness or exit code."""
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {
            "optional_reviews": [{"author": "gemini-code-assist", "state": "COMMENTED", "inline_count": 1}],
            "optional_reviews_unresolved": 0,
            "optional_reviews_resolved_unchanged": 1,
        },
    )
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 2},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    code = _status.run_status("42")
    captured = capsys.readouterr()
    out = _json.loads(captured.out)
    assert code == 0
    assert out["optional_reviews_resolved_unchanged"] == 1
    assert out["ready"] is True
    assert out["ready_blockers"] == []
    assert "original diff line remains current" in captured.err
    assert "does not block ready" in captured.err
    assert "explicit resolution rationale" in captured.err


def test_run_status_review_read_unknown_does_not_change_exit(monkeypatch, capsys):
    """AC1-ERR: an unknown review read leaves green + exit 0 intact."""
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": "unknown", "optional_reviews_unresolved": None},
    )
    # Coverage stubbed to a counted pass so the only blocker under test is the
    # unknown optional read.
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 2},
    )
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    code = _status.run_status("42")
    assert code == 0
    out = _json.loads(capsys.readouterr().out)
    assert out["green"] is True
    assert out["optional_reviews"] == "unknown"
    assert out["optional_reviews_unresolved"] is None
    assert out["ready"] is False


# ---- x-3a3f: status recomputes a missing coverage row ----


def test_status_recomputes_a_missing_coverage_row(monkeypatch, capsys, tmp_path):
    """A reviewed PR with no review_coverage event reports the same verdict
    merge would act on: the read fires the producer once (stubbed here to
    append the event a real binary would) instead of degrading to unknown."""
    import json

    from fno.pr import _reviews

    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True)

    def fake_verb(pr_number, cwd, head):
        events.write_text(
            json.dumps({
                "ts": "2026-08-14T03:00:00Z",
                "type": "review_coverage",
                "data": {"pr": pr_number, "coverage": "covered",
                         "review_state": "reviewed",
                         "reviewed_count": 1, "head_sha": "abc",
                         "verdicts": [{"name": "code-review",
                                       "producer": "local_attestation",
                                       "verdict": "reviewed",
                                       "reviewed_sha": "abc",
                                       "freshness": "fresh"}]},
            }) + "\n",
            encoding="utf-8",
        )
        return True, ""

    monkeypatch.setattr(_reviews, "_fire_review_coverage_verb", fake_verb)
    monkeypatch.setattr(_reviews, "_reviewed_sha_is_ancestor", lambda *args: True)
    # The status read resolves the project log from its cwd; point it at the
    # fixture. The lane is pinned on because recompute rides on it: a no-lane
    # repo must not fire the producer, and tmp_path resolves no real config.
    monkeypatch.setattr(_reviews, "_repo_root", lambda cwd=None: tmp_path)
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    code = _status.run_status("42", cwd=str(tmp_path))
    assert code == 0
    out = json.loads(capsys.readouterr().out)
    cov = out["review_coverage"]
    assert cov["coverage"] == "covered", cov
    assert cov["reviewed_count"] == 1
    assert cov["recompute"] == "recomputed", cov


def test_status_prints_degraded_recompute_reason_on_stderr(monkeypatch, capsys, tmp_path):
    """x-b56a: a recompute that degraded to unknown on an exhausted GraphQL
    quota must reach the human-readable stderr note, not just the JSON field.
    `unknown` from a dead read and `unknown` from "nobody reviewed this" are
    different facts; without the printed reason an operator cannot tell a
    quota window from a genuinely unreviewed PR."""
    import json

    from fno.pr import _reviews

    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    events = tmp_path / ".fno" / "events.jsonl"
    events.parent.mkdir(parents=True)

    def fake_verb(pr_number, cwd, head):
        events.write_text(
            json.dumps({
                "ts": "2026-08-14T03:00:00Z",
                "type": "review_coverage",
                "data": {"pr": pr_number, "coverage": "unknown",
                         "head_sha": "abc", "verdicts": []},
            }) + "\n",
            encoding="utf-8",
        )
        return True, "GraphQL quota exhausted (0 remaining, resets in ~14m)."

    monkeypatch.setattr(_reviews, "_fire_review_coverage_verb", fake_verb)
    monkeypatch.setattr(_reviews, "_repo_root", lambda cwd=None: tmp_path)
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    code = _status.run_status("42", cwd=str(tmp_path))
    assert code == 0
    cap = capsys.readouterr()
    out = json.loads(cap.out)
    cov = out["review_coverage"]
    assert cov["coverage"] == "unknown", cov
    assert "degraded to unknown" in cov["recompute"], cov
    assert "quota exhausted" in cap.err, cap.err


def test_status_recompute_failure_degrades_to_unknown(monkeypatch, capsys, tmp_path):
    """The verb unavailable -> the existing unknown sentinel, exit 0: a
    read-only report never goes non-zero on a coverage read."""
    import json

    from fno.pr import _reviews

    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    monkeypatch.setattr(
        _reviews, "_fire_review_coverage_verb", lambda *a, **k: (False, "fno-agents not found")
    )
    monkeypatch.setattr(_reviews, "_repo_root", lambda cwd=None: tmp_path)
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: True)
    code = _status.run_status("42", cwd=str(tmp_path))
    assert code == 0
    out = json.loads(capsys.readouterr().out)
    assert out["review_coverage"]["coverage"] == "unknown"


def test_no_lane_repo_never_fires_the_recompute(monkeypatch, capsys, tmp_path):
    """The merge gate skips the coverage read entirely on a no-lane repo
    because a missing row would fire the 120s recompute subprocess and append
    rows nobody acts on; status must not fire it either, now that ready
    ignores the conjunct there. A read that surfaces an EXISTING row is fine -
    only the producer spawn is the cost being gated."""
    import json

    from fno.pr import _reviews

    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: ({
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }, ""),
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )

    def must_not_fire(pr_number, cwd, head):
        raise AssertionError("recompute fired on a no-lane repo")

    monkeypatch.setattr(_reviews, "_fire_review_coverage_verb", must_not_fire)
    monkeypatch.setattr(_reviews, "_repo_root", lambda cwd=None: tmp_path)
    monkeypatch.setattr(_status, "_review_lane", lambda pr, cwd: False)
    code = _status.run_status("42", cwd=str(tmp_path))
    assert code == 0
    out = json.loads(capsys.readouterr().out)
    assert out["review_coverage"]["coverage"] == "unknown"
    assert out["ready"] is True


# ── x-a089: merge readiness must see a review that is RUNNING ────────────────
# `review_coverage` answers what verdicts EXIST for a head. It cannot see a
# review that is executing right now, and three PRs on 2026-08-22 read
# ready:true with an empty ready_blockers while one was still writing its fixes.


def _green_rollup():
    return [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}]


def _run_status_with_activity(monkeypatch, capsys, activity, *, state="OPEN"):
    """run_status on a green PR whose review-activity reading is `activity`."""
    seen = {}

    def _patch(name, value):
        monkeypatch.setattr(_status, name, value)

    _patch(
        "_fetch",
        lambda pr, cwd: (
            {
                "state": state,
                "statusCheckRollup": _green_rollup(),
                "headRefOid": "abc123",
                "headRefName": "feature/x-a089",
                "mergeable": "MERGEABLE",
            },
            "",
        ),
    )
    _patch(
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    _patch(
        "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 1, "head_sha": "abc123"},
    )
    _patch("_review_lane", lambda pr, cwd: False)

    def _activity(branch, head, cwd):
        seen["branch"] = branch
        seen["head"] = head
        return activity

    _patch("_review_activity", _activity)
    code = _status.run_status("42")
    cap = capsys.readouterr()
    return code, _json.loads(cap.out), cap.err, seen


def test_a_running_review_blocks_ready_even_with_no_lane_configured(monkeypatch, capsys):
    """The config makes this worse rather than better: with no review lane the
    coverage conjunct opts out entirely, so `ready` was true while a review was
    mid-flight. The in-flight conjunct is deliberately config-independent."""
    from fno.pr._review_hold import ReviewActivity

    _code, out, _err, seen = _run_status_with_activity(
        monkeypatch,
        capsys,
        ReviewActivity(
            True,
            "review_in_flight",
            "a review is in flight on feature/x-a089: held by reviewer:sess-1",
            {"holder": "reviewer:sess-1", "state": "live"},
            {"probed": False, "path": None, "dirty": None, "head": None, "note": "not reached"},
        ),
    )
    assert out["ready"] is False
    assert "review_in_flight" in out["ready_blockers"]
    assert out["review_activity"]["hold"]["holder"] == "reviewer:sess-1"
    # The branch is what the hold is keyed on; the sha is what the worktree
    # conjunct compares against.
    assert seen == {"branch": "feature/x-a089", "head": "abc123"}


def test_the_verdict_and_exit_code_are_untouched_by_the_conjunct(monkeypatch, capsys):
    """`ready` tightens; the CI verdict is authoritative and stays green."""
    from fno.pr._review_hold import ReviewActivity

    code, out, _err, _seen = _run_status_with_activity(
        monkeypatch,
        capsys,
        ReviewActivity(True, "worktree_dirty", "…", None,
                       {"probed": True, "path": "/wt/x", "dirty": True, "head": "abc123", "note": ""}),
    )
    assert code == 0
    assert out["verdict"] == "green" and out["green"] is True
    assert out["ready"] is False and out["ready_blockers"] == ["worktree_dirty"]


def test_a_clear_reading_is_still_reported(monkeypatch, capsys):
    """An empty `ready_blockers` was the whole complaint: a reader must be able
    to see that both probes RAN, not just that nothing came back."""
    from fno.pr._review_hold import ReviewActivity

    _code, out, _err, _seen = _run_status_with_activity(
        monkeypatch,
        capsys,
        ReviewActivity(False, "", "", None,
                       {"probed": True, "path": None, "dirty": None, "head": None,
                        "note": "no local worktree on this branch"}),
    )
    assert out["ready"] is True
    assert out["review_activity"]["worktree"]["probed"] is True
    assert out["review_activity"]["blocker"] == ""


def test_a_terminal_pr_is_never_probed(monkeypatch, capsys):
    """The guard protects what WOULD merge; a merged PR has no would-merge left."""
    from fno.pr._review_hold import ReviewActivity

    def _explode(branch, head, cwd):
        raise AssertionError("a terminal PR must not run the review probes")

    monkeypatch.setattr(_status, "_review_activity", _explode)
    _code, out, _err, _seen = _run_status_with_activity(
        monkeypatch,
        capsys,
        ReviewActivity(False, "", "", None, {}),
        state="MERGED",
    )
    assert out["review_activity"]["worktree"]["note"] == "not asked: PR is terminal"


def test_a_thrown_probe_blocks_rather_than_clearing(monkeypatch, capsys):
    """Fail-closed, like every other hold read here."""
    from fno.pr import _review_hold

    monkeypatch.setattr(
        _review_hold, "review_activity", lambda *a, **kw: (_ for _ in ()).throw(RuntimeError("boom"))
    )
    activity = _REAL_REVIEW_ACTIVITY("feature/x-a089", "abc123", None)
    assert activity.blocked is True
    assert activity.blocker == _review_hold.REVIEW_HOLD_UNREADABLE
    assert "refusing to assume no review is running" in activity.detail


def test_a_missing_head_branch_is_a_missing_input_not_a_failed_probe(monkeypatch):
    """A degraded fetch that omitted headRefName has no hold key and no
    worktree to match. That is nothing to ask, not a probe that died."""
    activity = _REAL_REVIEW_ACTIVITY("", "abc123", None)
    assert activity.blocked is False
    assert activity.worktree["note"] == "no head branch on the PR read"
