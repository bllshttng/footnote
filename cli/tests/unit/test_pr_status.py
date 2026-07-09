"""Tests for `fno pr status` verdict logic (x-8b64 G).

The non-trivial part is classifying a mixed statusCheckRollup: CheckRun entries
carry status+conclusion (conclusion empty until COMPLETED), StatusContext
entries carry only state. The Boundary cases: an in-progress check is *pending*
not red, and an empty rollup is *unknown* not red.
"""
from __future__ import annotations

import json as _json

from fno.pr import _reviews, _status
from fno.pr._proc import Result


def test_all_pass_is_green():
    rollup = [
        {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"context": "legacy", "state": "SUCCESS"},
    ]
    verdict, code, counts = _status.verdict_for(rollup)
    assert verdict == "green"
    assert code == 0
    assert counts == {"total": 2, "pass": 2, "fail": 0, "pending": 0}


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


def test_run_status_emits_json_and_code(monkeypatch, capsys):
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: {
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        },
    )
    # Stub the review read (no gh) so the frozen contract is deterministic.
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    code = _status.run_status("42")
    assert code == 0
    import json

    out = json.loads(capsys.readouterr().out)
    assert out == {
        "pr": "42",
        "verdict": "green",
        "settled": True,
        "green": True,
        "pr_state": "OPEN",
        "checks": {"total": 1, "pass": 1, "fail": 0, "pending": 0},
        "optional_reviews": [],
        "optional_reviews_unresolved": 0,
        "ready": True,
    }


def test_run_status_fetch_failure_is_error(monkeypatch, capsys):
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: None)
    code = _status.run_status("99")
    assert code == 4
    import json

    out = json.loads(capsys.readouterr().out)
    assert out["verdict"] == "error"
    assert out["settled"] is False


# --- x-705b: optional-review signal on `fno pr status` ---------------------
#
# These exercise read_optional_review_state with a fake `runner` (no gh) plus the
# run_status integration. "Optional" resolves to the hardcoded bots regardless of
# config (config read is best-effort and wrapped), so gemini/codex are stable.

_URL = "https://github.com/o/r/pull/42"


def _fake_runner(*, reviews, threads, view_ok=True, graphql_ok=True):
    """Dispatch gh calls: `pr view` -> reviews+url, `api graphql` -> threads."""

    def runner(cmd, *, cwd=None, timeout=None, **_):
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


def _thread(login, resolved):
    return {"isResolved": resolved, "comments": {"nodes": [{"author": {"login": login}}]}}


def test_ac1_hp_unresolved_optional_findings_surface():
    """AC1-HP: green PR + gemini COMMENTED with 2 unresolved threads."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(
            reviews=[{"author": {"login": "gemini-code-assist[bot]"}, "state": "COMMENTED"}],
            threads=_threads_payload([
                _thread("gemini-code-assist[bot]", False),
                _thread("gemini-code-assist[bot]", False),
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
                _thread("gemini-code-assist[bot]", True),
                _thread("gemini-code-assist[bot]", True),
            ]),
        ),
    )
    assert state["optional_reviews_unresolved"] == 0
    assert state["optional_reviews"][0]["author"] == "gemini-code-assist"


def test_ac1_fr_non_optional_review_excluded():
    """AC1-FR/US3: an arbitrary human login is neither listed nor counted."""
    state = _reviews.read_optional_review_state(
        "42",
        runner=_fake_runner(
            reviews=[{"author": {"login": "some-human"}, "state": "CHANGES_REQUESTED"}],
            threads=_threads_payload([_thread("some-human", False)]),
        ),
    )
    assert state["optional_reviews"] == []
    assert state["optional_reviews_unresolved"] == 0


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
    assert state == {"optional_reviews": "unknown", "optional_reviews_unresolved": None}


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
    assert state == {"optional_reviews": "unknown", "optional_reviews_unresolved": None}


def test_graphql_errors_envelope_degrades_to_unknown():
    """A GraphQL error envelope (rc=0, `errors` set) is unavailable, not empty."""
    def runner(cmd, *, cwd=None, timeout=None, **_):
        if "graphql" in cmd:
            return Result(0, _json.dumps({"errors": [{"message": "nope"}]}), "")
        return Result(0, _json.dumps({"url": _URL, "reviews": []}), "")

    state = _reviews.read_optional_review_state("42", runner=runner)
    assert state == {"optional_reviews": "unknown", "optional_reviews_unresolved": None}


def test_non_object_json_degrades_to_unknown():
    """US4: a valid-but-non-object JSON body degrades, never AttributeErrors."""
    def runner(cmd, *, cwd=None, timeout=None, **_):
        return Result(0, _json.dumps(["not", "an", "object"]), "")

    state = _reviews.read_optional_review_state("42", runner=runner)
    assert state == {"optional_reviews": "unknown", "optional_reviews_unresolved": None}


def test_us2_green_with_unresolved_optional_still_exits_zero(monkeypatch, capsys):
    """US2/AC1-UI: an unresolved optional finding never changes the exit code."""
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: {
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        },
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {
            "optional_reviews": [{"author": "gemini-code-assist", "state": "COMMENTED", "inline_count": 2}],
            "optional_reviews_unresolved": 2,
        },
    )
    code = _status.run_status("42")
    assert code == 0  # green exit unchanged despite an unresolved optional finding
    out = _json.loads(capsys.readouterr().out)
    assert out["green"] is True
    assert out["optional_reviews_unresolved"] == 2
    assert out["ready"] is False  # green but not ready: the actionable signal


def test_run_status_review_read_unknown_does_not_change_exit(monkeypatch, capsys):
    """AC1-ERR: an unknown review read leaves green + exit 0 intact."""
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: {
            "state": "OPEN",
            "statusCheckRollup": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        },
    )
    monkeypatch.setattr(
        _status,
        "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": "unknown", "optional_reviews_unresolved": None},
    )
    code = _status.run_status("42")
    assert code == 0
    out = _json.loads(capsys.readouterr().out)
    assert out["green"] is True
    assert out["optional_reviews"] == "unknown"
    assert out["optional_reviews_unresolved"] is None
    assert out["ready"] is False
