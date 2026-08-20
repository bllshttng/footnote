"""`fno pr status <n>` - one authoritative CI verdict for a PR (x-8b64 G).

Agents kept re-deriving CI-green from `statusCheckRollup` by hand (or trusting
`gh pr checks`, which disagrees with the rollup). This computes a single
settled/green/red verdict from the check rollup, handling
the in-progress case (a CheckRun with `status != COMPLETED` has an empty
`conclusion` and must read as *pending*, never red) and the no-checks case
(verdict `unknown`, never red). The rollup arrives over REST (`fno.pr._rest`)
so the read spends the idle core budget, never the shared GraphQL one.

Exit codes (so a caller can branch without re-parsing the JSON). The code is
always the VERDICT's code, which answers "may this merge"; the `settled` field
answers the different question "is anything left to wait for", and the two are
allowed to disagree - a cancelled latest run is red AND unsettled:
    0  green    - every check passed
    1  red      - at least one check failed or was cancelled
    2  pending  - a check is still queued/running
    3  unknown  - no checks on the PR
    4  error    - could not fetch PR state (no PR, gh error, bad JSON)
    127 gh missing
"""
from __future__ import annotations

import json
import os
from typing import Any, Optional, Sequence

from fno.pr._proc import ToolMissing
from fno.pr._reviews import (
    _NOT_ASKED_COVERAGE,
    _UNKNOWN_COVERAGE,
    read_optional_review_state,
    read_review_coverage,
)

# Rollup states that count as a pass (jq parity with _verify._PASS_STATES).
_PASS_STATES = {"SUCCESS", "NEUTRAL", "SKIPPED"}
# Terminal non-pass conclusions / StatusContext states.
_FAIL_STATES = {
    "FAILURE",
    "TIMED_OUT",
    "CANCELLED",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
    "STALE",
    "ERROR",
}
# Conclusions that prove a result EXISTS. Deliberately _FAIL_STATES minus
# CANCELLED and STALE: those two say the run was taken away, not that it
# reached a verdict, so the answer to "wait or act" is still "wait for, or
# trigger, a newer run". They stay in _FAIL_STATES because the verdict is red.
_SETTLED_STATES = _PASS_STATES | (_FAIL_STATES - {"CANCELLED", "STALE"})


def _alt(*vals: Any) -> Any:
    for v in vals:
        if v not in (None, ""):
            return v
    return vals[-1] if vals else None


def _classify(check: dict) -> str:
    """Classify one rollup entry as 'pass' | 'fail' | 'pending'.

    A CheckRun carries `status` (QUEUED/IN_PROGRESS/COMPLETED) and only fills
    `conclusion` once COMPLETED - so an in-progress run has `conclusion == ""`
    and must be pending, not red (the plan's Boundary). A StatusContext carries
    only `state` (SUCCESS/PENDING/FAILURE/ERROR) and no `status`.
    """
    status = str(check.get("status") or "").upper()
    if status and status != "COMPLETED":
        # In-progress CheckRun: conclusion is still empty.
        return "pending"
    raw = str(_alt(check.get("conclusion"), check.get("state"), "")).upper()
    if raw in _PASS_STATES:
        return "pass"
    if raw in _FAIL_STATES:
        return "fail"
    # PENDING / EXPECTED / REQUESTED / unknown / empty -> not settled.
    return "pending"


def _has_settled_marker(check: dict) -> bool:
    """True iff this entry carries a POSITIVE marker that a result exists.

    Never the absence of a pending sibling: an empty rollup and an all-green
    one both have zero pending entries, and only one of them is decided.
    """
    status = str(check.get("status") or "").upper()
    if status and status != "COMPLETED":
        return False
    raw = str(_alt(check.get("conclusion"), check.get("state"), "")).upper()
    return raw in _SETTLED_STATES


def _entry_ts(check: dict) -> str:
    """Recency key for 'latest run per name' = when the run was TRIGGERED, not
    when it finished. A superseding run always STARTS later but need not finish
    later (a fast rerun can complete before a slow superseded run still winding
    down), so keying on startedAt - createdAt for a StatusContext, never
    completedAt - makes 'latest by timestamp' mean 'latest attempt'. An
    in-progress run has a startedAt and empty completedAt, so it still sorts.
    Missing both -> '' (sorts oldest, loses to any timestamped sibling).
    ISO-8601 strings sort chronologically as plain strings.
    """
    return str(_alt(check.get("startedAt"), check.get("createdAt"), ""))


def _latest_per_name(rollup: Sequence[dict]) -> list[dict]:
    """Keep only the latest run per check name/context.

    A force/amend push leaves superseded runs (e.g. a CANCELLED CI) in the
    rollup beside the fresh ones; classifying all of them counts a stale
    CANCELLED as a live fail. Grouping by name and keeping the max-startedAt
    entry drops the stale run, so a superseded CANCELLED loses to a newer
    same-name run while a genuinely-cancelled *latest* run stays.

    Only a STRICTLY-newer timestamp replaces the kept entry. On a tie (equal
    timestamps, or both missing) the tie-break is fail-closed: a failing entry
    is never dropped by a same-time non-fail, so a superseded pass can never
    hide a real fail even when gh emits no usable ordering (the load-bearing
    invariant). A tie between two non-fails keeps the first-seen (deterministic).

    The key discriminates a CheckRun's `name` space from a StatusContext's
    `context` space, so two DIFFERENT checks that happen to share a literal
    string are never merged (which could drop a genuine fail); same-kind reruns
    still group on the shared name. An entry with neither key is never merged.
    """
    latest: dict[Any, dict] = {}
    order: list[Any] = []
    unkeyed: list[dict] = []
    for c in rollup:
        nm = c.get("name")
        ctx = c.get("context")
        if nm not in (None, ""):
            key: Any = ("check", nm)
        elif ctx not in (None, ""):
            key = ("status", ctx)
        else:
            unkeyed.append(c)
            continue
        existing = latest.get(key)
        if existing is None:
            latest[key] = c
            order.append(key)
        else:
            tc, te = _entry_ts(c), _entry_ts(existing)
            if tc > te:
                latest[key] = c
            elif tc == te and _classify(existing) != "fail" and _classify(c) == "fail":
                # Tie ONLY (equal/missing timestamps): a fail must never be
                # dropped by a same-time non-fail. A strictly-OLDER fail still
                # loses (the `tc == te` guard is load-bearing - without it a
                # superseded older CANCELLED would re-hide as a false red).
                latest[key] = c
    return [latest[k] for k in order] + unkeyed


def _fetch(pr: str, cwd: Optional[str]) -> "tuple[Optional[dict], str]":
    """Return (parsed json, reason). ``reason`` is empty on success.

    The reason is carried rather than dropped because a bare ``verdict: error``
    is unactionable. The cause is always on gh's stderr and used to be discarded
    here, so a caller saw the same four fields for a deleted PR, a network
    failure, and an exhausted API quota.

    The reads are REST: `gh pr view` spends the per-USER GraphQL quota
    that every watcher on the machine shares, and its exhaustion blinded the
    stop hook for whole reset windows while core REST sat untouched. See
    ``fno.pr._rest``; the GraphQL reader this replaced lived inline here.
    """
    from fno.pr._rest import fetch_pr_rest

    return fetch_pr_rest(pr, cwd)


def verdict_for(rollup: Sequence[dict]) -> tuple[str, int, dict]:
    """Pure verdict computation. Returns (verdict, exit_code, counts).

    Classifies only the latest run per check name so a superseded CANCELLED run
    (left in the rollup by a force/amend push) no longer yields a false red.
    `counts["total"]` is the deduped count over the WHOLE rollup, which carries
    two different kinds of row: GitHub check-runs (the `name` key, produced by
    Actions and Checks-API apps) and commit StatusContexts (the `context` key,
    posted to the statuses endpoint). `counts["check_runs"]` and
    `counts["statuses"]` split that total, because a reader who compares
    `total` against `gh api .../check-runs` sees a phantom gap otherwise: that
    endpoint never returns statuses. Measured on PR 981 - the tally said 13,
    the check-runs endpoint named 10 jobs, and the 3-row gap was
    fno's own statuses (stacked-base-guard, fno/review-coverage). The two
    sub-counts need not sum to `total`: a rollup row carrying neither key is
    counted in neither (it is also never deduped).
    `counts["unsettled"]` counts latest runs with NO settled marker (an absent
    result: cancelled, stale, still running), and `settled` is derived from it
    positively elsewhere - never from the absence of a pending run.

    A would-be-green tally is refused when NO entry is a real check-run (the
    `name` key, produced by GitHub Actions/apps via the check-runs API) -
    x-4271: a conflicting PR (mergeable_state dirty) gets zero workflow runs,
    but fno's own self-published StatusContexts (review-coverage,
    stacked-base-guard) still post and can all pass, so an all-`context`
    rollup read as green with no CI having run at all. A fail or pending
    StatusContext is still a real, actionable signal and stays red/pending.
    """
    deduped = _latest_per_name(rollup)
    counts = {
        "total": len(deduped),
        "check_runs": 0,
        "statuses": 0,
        "pass": 0,
        "fail": 0,
        "pending": 0,
        "unsettled": 0,
    }
    for c in deduped:
        counts[_classify(c)] += 1
        if not _has_settled_marker(c):
            counts["unsettled"] += 1
        if c.get("name") not in (None, ""):
            counts["check_runs"] += 1
        elif c.get("context") not in (None, ""):
            counts["statuses"] += 1
    real_check_runs = counts["check_runs"]
    if not deduped:
        return ("unknown", 3, counts)
    if counts["fail"]:
        return ("red", 1, counts)
    if counts["pending"]:
        return ("pending", 2, counts)
    if not real_check_runs:
        # Known tradeoff, not footnote's own blast radius: a repo whose ONLY
        # real CI still rides the legacy commit-status API (no GitHub Actions,
        # no Checks-API app) would never clear this and would hold forever
        # under `require_checks_pass` (unknown holds, never fails - see
        # _merge.py's `_checks_verdict` caller). footnote's own workflows
        # (guards.yml et al.) are all Actions/CheckRuns, so this repo never
        # hits it; a fork that genuinely needs status-only CI as its sole
        # signal should route around this via `require_checks_pass=false`.
        return ("unknown", 3, counts)
    return ("green", 0, counts)


def coverage_recompute_note(coverage: dict) -> None:
    """Print the coverage recompute note on stderr.

    Shared by the live read and the cache serve, so a degraded reason reaches
    every terminal rather than only the one session whose read produced the
    row. The bare success note stays silent: it adds nothing the coverage
    line above does not already say, and a prefix that fires on every first
    poll trains a reader to skip the line where a reason appears.
    """
    import sys

    note = coverage.get("recompute")
    if note and note != "recomputed":
        sys.stderr.write(f"note: coverage recompute: {note}\n")


def _review_lane(pr: str, cwd: Optional[str]) -> bool:
    """Whether the merge gate's coverage guard engages for this PR.

    The SAME lane predicate ``fno pr merge`` reads (``_review_lane_configured``
    in ``_merge``): a stock install with no lane opts out of review there, so
    ``ready`` must opt out with it or the two verbs answer opposite ways - the
    exact divergence this conjunction exists to remove, just inverted onto the
    no-lane repo (status refusing forever at uncovered 0 while merge merges).
    Fail-closed (True) on any error, like the merge side.
    """
    try:
        from fno.pr._merge import _review_lane_configured

        return bool(_review_lane_configured(cwd or os.getcwd(), int(pr)))
    except Exception:
        return True


def _merge_hold_reason(pr: str, cwd: Optional[str]) -> Optional[str]:
    from fno.pr._hold import merge_hold_reason

    try:
        return merge_hold_reason(int(pr), cwd or os.getcwd())
    except Exception as exc:  # noqa: BLE001 - hold visibility fails closed
        return f"dispatch-hold-invalid: {exc}; refusing to assume unheld"


def _ready_blockers(
    green: bool,
    verdict: str,
    unresolved: object,
    coverage: dict,
    review_lane: bool = True,
    *,
    head: str = "",
    code_review_required: bool = False,
    mergeable: Optional[str] = None,
) -> list[str]:
    """Which conjuncts of ``ready`` fail, in a stable order.

    A bare ``ready: false`` has one explanation per conjunct (CI red, an
    unresolved optional finding, coverage unknown or uncovered) and a reader
    cannot tell them apart; the list is the positive marker that names what is
    holding. ``unknown`` coverage blocks and is named as its own blocker: the
    reason a read returned unknown is a separate question (x-b56a), this only
    reports that the answer is missing. Fail-closed everywhere: an unset
    unresolved count blocks as ``optional_reviews_unknown``, and the coverage
    conjunct engages wherever the merge gate's coverage guard would
    (``review_lane``; a repo with no review lane has no coverage answer to
    fail).

    The coverage conjuncts themselves are the merge gate's own, read through
    ``_coverage_gate.covered_conjuncts`` - one copy, never a restatement - so
    ``ready`` cannot pass a row ``fno pr merge`` refuses (missing local pass,
    stale head pin).

    A TERMINAL PR (merged or closed) is exempt from the coverage conjunct: the
    gate guards what would merge, and a PR merged out-of-band (UI, bare gh) has
    no "would" left to guard. That exemption arrives as ``review_lane=False``
    from the caller's terminal arm and needs no conjunct of its own here. It
    had one - a ``merged`` flag - and it was decorative: ``merged`` is only
    ever True inside the branch that already sets ``review_lane=False``, so it
    never changed an outcome while reading like the protection its name
    promised. A guard that cannot fire is worse than no guard, because the
    next reader budgets for it.
    """
    blockers: list[str] = []
    # x-4271: `mergeable` is None only when the caller never asked (old test
    # stubs, a degraded fetch that omitted the field) - never block on that,
    # only on a GitHub-supplied answer that isn't the positive "MERGEABLE".
    # `_map_mergeable` already turns a null (still computing) into the string
    # "UNKNOWN", so this also fails closed on "still computing", not open.
    if mergeable not in (None, "MERGEABLE"):
        blockers.append(f"not_mergeable_{str(mergeable).lower()}")
    if not green:
        blockers.append(f"ci_{verdict}")
    if unresolved is None or not isinstance(unresolved, int):
        # None is the read's own "unknown"; a non-int is a contract violation.
        # Both fail closed as unknown rather than TypeError or a silent pass.
        blockers.append("optional_reviews_unknown")
    elif unresolved > 0:
        blockers.append("optional_reviews_unresolved")
    if review_lane:
        cov_word = coverage.get("coverage")
        if cov_word == "unknown":
            blockers.append("review_coverage_unknown")
        else:
            from fno.pr._coverage_gate import covered_conjuncts

            ok, failed = covered_conjuncts(coverage, head, code_review_required)
            if not ok:
                blockers.append(f"review_coverage_{failed}")
    return blockers


def run_status(pr: str, cwd: Optional[str] = None, *, review_reader=None) -> int:
    """Print a one-line JSON verdict for PR `pr`; return the exit code.

    The exit code is ALWAYS the CI verdict's code (0/1/2/3/4/127) - the review
    fields are additive and advisory (optional stays advisory; an unresolved
    optional finding on a green PR still exits 0). ``ready`` is the one field
    that conjoins them all (CI green, optional findings resolved, coverage a
    counted pass), with ``ready_blockers`` naming any conjunct that failed;
    a caller branching on the exit code is untouched. ``review_reader`` is
    injectable for tests; it defaults to the real time-boxed read.
    """
    import sys

    pr_json, reason = _fetch(pr, cwd)
    if pr_json is None:
        sys.stdout.write(
            json.dumps(
                {
                    "pr": pr,
                    "verdict": "error",
                    "settled": False,
                    "green": False,
                    "reason": reason,
                }
            )
            + "\n"
        )
        return 4

    rollup = pr_json.get("statusCheckRollup") or []
    verdict, code, counts = verdict_for(rollup)
    green = verdict == "green"

    # A terminal PR (round 3) has no would-merge left: the coverage conjunct
    # guards what WOULD merge, and the probes that feed it are live reads a
    # closed PR can still burn - `gh pr view --json reviews` and a 120s
    # recompute against a PR that will never merge again. Skip all of them;
    # the report prints the no-pending answers ([] / 0) rather than `unknown`,
    # because nothing was failed, it was deliberately not asked.
    is_terminal = (pr_json.get("state") or "").upper() in ("MERGED", "CLOSED")
    if is_terminal:
        # Any-typed to match the probe arms below, whose reads return the
        # same untyped dicts; a first binding of `0` would narrow the
        # variable to int and fail the reassignments' type check.
        reviews: Any = {"optional_reviews": [], "optional_reviews_unresolved": 0}
        unresolved: Any = 0
        coverage: Any = dict(_NOT_ASKED_COVERAGE)
        review_lane = False
        code_review_required = False
        hold_reason = None
    else:
        # Additive review signal (x-705b): computed AFTER the authoritative CI
        # verdict so a slow/failed review read can never delay or corrupt it.
        # Any failure degrades to "unknown"/None and leaves the CI verdict +
        # exit code untouched.
        reader = review_reader or read_optional_review_state
        try:
            reviews = reader(pr, cwd)
        except Exception:
            reviews = {"optional_reviews": "unknown", "optional_reviews_unresolved": None}
        unresolved = reviews.get("optional_reviews_unresolved")

        # x-0eaf: coverage signal, same additive/fail-open discipline as the
        # optional review read above. Read from the review_coverage event so a
        # human and the loop see one number (Ownership: Rust computes, Python
        # reads). Recomputed once when no usable row exists (x-3a3f), so a
        # human report and the merge gate act on the same number instead of
        # status saying "no coverage" for a PR merge would clear after one
        # recompute. The PR head rides in from _fetch: without it the verb
        # would pin the emitted row to the LOCAL checkout's HEAD, planting a
        # wrong-head row both gates then disagree on. The lane answer comes
        # BEFORE the coverage read, not beside it: on a no-lane repo the
        # read's recompute is a 120s subprocess that appends coverage rows
        # nobody acts on - the exact cost `fno pr merge` skips on this same
        # boundary - so a conjunct ready ignores must not fire it either.
        #
        # ONE probe chain where two ran: required implies lane (a configured
        # code-review reviewer IS a lane, and the self-review floor that makes
        # a code payload required is the same floor that makes the lane
        # exist), so the required probe answers both when true and only a
        # clean not-required verdict pays for the lane probe. Each predicate
        # keeps its own fail-closed direction on a thrown probe.
        code_review_required = False
        try:
            from fno.pr import _merge

            code_review_required = bool(
                _merge._code_review_attestation_required(cwd or os.getcwd(), int(pr))
            )
        except Exception:  # noqa: BLE001 - fail closed, like the gate
            code_review_required = True
        review_lane = code_review_required or _review_lane(pr, cwd)
        try:
            coverage = read_review_coverage(
                int(pr), cwd, head=pr_json.get("headRefOid"), recompute=review_lane
            )
        except Exception:
            # The producer's own sentinel, not a copy of it: a second literal
            # here is a shape that drifts the moment a key is added on one
            # side only.
            coverage = dict(_UNKNOWN_COVERAGE)
        hold_reason = _merge_hold_reason(pr, cwd)

    blockers = _ready_blockers(
        green,
        verdict,
        unresolved,
        coverage,
        review_lane,
        head=pr_json.get("headRefOid") or "",
        code_review_required=code_review_required,
        # A terminal PR has no would-merge left, like the coverage conjunct
        # above - a merged/closed PR's mergeable field is stale and must not
        # hold a report that changes nothing.
        mergeable=None if is_terminal else pr_json.get("mergeable"),
    )
    if hold_reason:
        blockers.append("dispatch_hold")
    sys.stdout.write(
        json.dumps(
            {
                "pr": pr,
                # The commit this verdict describes, so a caller can pin the
                # answer to a head instead of trusting it across a push.
                "head": pr_json.get("headRefOid"),
                "verdict": verdict,
                # total > 0 is load bearing: an empty rollup and an all-green
                # one both have zero unsettled entries, and only one of them
                # is decided. Existence must be stated, not inherited.
                # verdict != unknown too (x-4271): a zero-real-check-run
                # rollup can have zero unsettled entries (every StatusContext
                # already settled) while still being an undecided read.
                "settled": verdict != "unknown" and counts["total"] > 0
                and counts["unsettled"] == 0,
                "green": green,
                "pr_state": pr_json.get("state"),
                "mergeable": pr_json.get("mergeable"),
                "checks": counts,
                "optional_reviews": reviews.get("optional_reviews", "unknown"),
                "optional_reviews_unresolved": unresolved,
                "review_coverage": coverage,
                "dispatch_hold": hold_reason,
                # The obvious "read this, not green": ready iff CI is green AND
                # no optional finding is unresolved AND review coverage is a
                # counted pass. Coverage joined the conjunction because `fno
                # pr merge` already read it (x-e601): with it absent here, the
                # two verbs answered opposite ways from one payload and every
                # ready: true PR refused at the merge gate. The blockers list
                # names WHICH conjunct failed - a bare false has one
                # explanation per conjunct and a reader would have to guess.
                # `covered and reviewed_count > 0` rather than the word alone:
                # historical events serialize a real zero as `covered`, and
                # every existing consumer tests both.
                "ready": not blockers,
                "ready_blockers": blockers,
            }
        )
        + "\n"
    )
    # Same discipline as the unresolved-findings note below: a number a human
    # would misread gets its instruction beside it, on stderr. An unsettled
    # entry has two distinct causes and they need distinct instructions: a
    # completed-but-markerless entry (cancelled or stale) says push again,
    # while a still-running entry says wait. Conflating them told a human on
    # an ordinary in-progress PR to rerun a workflow that never failed. Both
    # notes read the actual `verdict`, never a hardcoded "red": a run of
    # unsettled entries that are all still-running settles as `pending`, not
    # `red`, and the note must not claim otherwise.
    if counts.get("unsettled"):
        unsettled_now = [c for c in _latest_per_name(rollup) if not _has_settled_marker(c)]
        absent = [
            c for c in unsettled_now if str(c.get("status") or "").upper() in ("", "COMPLETED")
        ]
        running = [c for c in unsettled_now if c not in absent]
        if absent:
            names = ", ".join(str(c.get("name") or c.get("context") or "?") for c in absent)
            sys.stderr.write(
                f"note: {len(absent)} check(s) produced no result (cancelled or stale): "
                f"{names}. The verdict is {verdict}, and settled stays false because a "
                "cancelled run is an ABSENT result, not a terminal one. "
                "Push again or rerun the workflow. Do not read this PR as decided.\n"
            )
        if running:
            names = ", ".join(str(c.get("name") or c.get("context") or "?") for c in running)
            sys.stderr.write(
                f"note: {len(running)} check(s) are still queued or running: {names}. "
                f"The verdict is {verdict}, and settled stays false until every latest run "
                "finishes. Wait for the run to finish. Do not start a new one.\n"
            )
    # Say what to DO about a non-zero counter, on stderr so the JSON contract is
    # untouched. Answering a finding does NOT clear it: a review thread stays
    # unresolved until it is resolved EXPLICITLY, so a PR whose every finding has
    # a reply can sit at ready=false indefinitely while reading as handled. That
    # cost a session tonight, and it is invisible from this number alone - which
    # is exactly why the instruction belongs in the output that prints the number
    # rather than in a PR body nobody re-reads.
    if isinstance(unresolved, int) and unresolved > 0:
        sys.stderr.write(
            f"note: {unresolved} optional review finding(s) unresolved, so ready "
            "stays false. A REPLY DOES NOT RESOLVE A THREAD. Fix each one, or "
            "answer it in-thread, then resolve the thread explicitly: the "
            '"Resolve conversation" button, or `gh api graphql -f query='
            "'mutation($t: ID!){resolveReviewThread(input:{threadId: $t})"
            "{thread{isResolved}}}' -F t=<threadId>` (thread ids come from "
            "`reviewThreads` on the pullRequest).\n"
        )

    # Coverage used to print a word and a number with no way to check either.
    # "covered, reviewed_count 2" rendered identically whether the reviewers had
    # read this commit or one from twelve hours and two commits ago, and the
    # word is the half a reader trusts. Name the commit that was covered, and
    # name any reviewer whose verdict sits on an older one.
    cov_head = coverage.get("head_sha")
    stale = coverage.get("stale_verdicts") or []
    if cov_head or stale:
        line = f"note: review coverage {coverage.get('coverage')}"
        if coverage.get("reviewed_count") is not None:
            line += f" ({coverage['reviewed_count']} reviewed"
            self_n = coverage.get("self_attested_count")
            if self_n:
                line += f", {self_n} self-attested"
            line += ")"
        if cov_head:
            line += f" computed at {str(cov_head)[:8]}"
        sys.stderr.write(line + "\n")
    for v in stale:
        sys.stderr.write(
            f"note: {v.get('name')} ({v.get('producer')}) reviewed "
            f"{str(v.get('reviewed_sha') or 'an unknown commit')[:8]}, whose code no longer "
            "matches HEAD - that verdict does not count. Ask it to re-read.\n"
        )
    # `unknown` from a degraded gh read and `unknown` from "nobody reviewed
    # this" are different facts; the recompute note is the only thing that
    # separates them, and the JSON field alone would never reach a terminal.
    coverage_recompute_note(coverage)
    return code


def main(argv: Sequence[str]) -> int:
    args = [a for a in argv if not str(a).startswith("--")]
    refresh = any(str(a) in ("--refresh", "--no-cache") for a in argv)
    if not args:
        import sys

        sys.stderr.write("usage: fno pr status <pr-number> [--refresh]\n")
        return 2
    try:
        from fno.pr._cache import cached_status

        # The CLI chokepoint goes through the coalescing cache: N sessions
        # polling one PR issue one network read per TTL. The
        # library entry (run_status) stays uncached for programmatic callers
        # and tests.
        return cached_status(str(args[0]), refresh=refresh)
    except ToolMissing:
        import sys

        sys.stderr.write("fno pr status: gh not found on PATH\n")
        return 127
