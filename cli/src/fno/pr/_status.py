"""`fno do pr status <n>` - one authoritative CI verdict for a PR (G).

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
import sys
from pathlib import Path
from typing import Any, Collection, Optional, Sequence

from fno.pr._proc import ToolMissing
from fno.pr._check_supersession_generated import latest_per_name as _latest_per_name
from fno.pr._reviews import (
    _NOT_ASKED_COVERAGE,
    _UNKNOWN_COVERAGE,
    COVERAGE_STATUS_CONTEXTS,
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

# The canonical collection lives in _reviews (the publisher); the parity
# script pins its consts against the Rust twin, so this is a name, not a
# second spelling.


def without_coverage_statuses(
    rollup: Sequence[dict], contexts: Collection[str] = COVERAGE_STATUS_CONTEXTS
) -> list[dict]:
    """Remove review-coverage projections before classifying generic CI.

    `contexts` parameterizes the drop so the merge verdict's ignore set reuses
    this one filter instead of re-spelling it inline.
    """
    return [
        check
        for check in rollup
        if check.get("context") not in contexts and check.get("name") not in contexts
    ]


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

    Counts vocabulary, the attribution split, the settled-marker rule, and
    the zero-real-check-run refusal are narrated in
    docs/architecture/pr-status-verdict.md (`verdict_for`).
    """
    deduped = _latest_per_name(rollup)
    counts = {
        "total": len(deduped),
        "check_runs": 0,
        "statuses": 0,
        "fail_check_runs": 0,
        "fail_statuses": 0,
        "pass": 0,
        "fail": 0,
        "pending": 0,
        "unsettled": 0,
        "unsettled_fail": 0,
    }
    for c in deduped:
        kind = _classify(c)
        counts[kind] += 1
        if not _has_settled_marker(c):
            counts["unsettled"] += 1
            if kind == "fail":
                counts["unsettled_fail"] += 1
        if c.get("name") not in (None, ""):
            counts["check_runs"] += 1
            if kind == "fail":
                counts["fail_check_runs"] += 1
        elif c.get("context") not in (None, ""):
            counts["statuses"] += 1
            if kind == "fail":
                counts["fail_statuses"] += 1
    if not deduped:
        return ("unknown", 3, counts)
    if counts["fail"]:
        return ("red", 1, counts)
    if counts["pending"]:
        return ("pending", 2, counts)
    if not counts["check_runs"]:
        # Zero real check-runs never reads green (docs/architecture/
        # pr-status-verdict.md, `verdict_for` tradeoff paragraph).
        return ("unknown", 3, counts)
    return ("green", 0, counts)


# Conclusions that count as a real failed attempt. CANCELLED stays out: a
# taken-away run is not a concluded failure (cf. `verdict_for`'s unsettled_fail).
_RERUN_FAIL_CONCLUSIONS = ("failure", "timed_out", "startup_failure")
_NO_RECOVERY: dict = {"recovered": False, "failed": []}
_RERUN_MAX_RUNS = 20  # one status read must not become a hundred gh calls


def _recovery_from_run_rows(run_rows, attempts_of, failed_jobs_of) -> dict:
    """Pure over pre-computed rows (no gh). Recovery = latest attempt passed,
    an earlier attempt failed; `failed` names jobs, optional diagnostics."""
    recovered = False
    failed: list[str] = []
    for row in run_rows:
        if str(row.get("conclusion") or "") != "success":
            continue  # only a run that now passes can have recovered
        try:
            latest = int(row.get("run_attempt") or 1)
        except (TypeError, ValueError):
            continue
        run_id = str(row.get("id") or "").strip()
        if latest <= 1 or not run_id:
            continue  # a first-attempt pass never failed
        for attempt in attempts_of(run_id):
            try:
                n = int(attempt.get("run_attempt") or 0)
            except (TypeError, ValueError):
                continue
            if (
                0 < n < latest
                and str(attempt.get("conclusion") or "") in _RERUN_FAIL_CONCLUSIONS
            ):
                recovered = True
                failed.extend(failed_jobs_of(run_id, n))
    return {"recovered": recovered, "failed": failed}


def rerun_recovery(
    pr_number,
    cwd: Optional[str] = None,
    sha: Optional[str] = None,
    runs: Optional[list] = None,
) -> dict:
    """Rerun-recovery fact for a PR head: ``{recovered, failed}``.

    A re-run-recovered failure reads green to `verdict_for`; this names it.
    ANY read error fails open: a fact beside the verdict, never a second red.
    `sha` skips the PR-info read when the caller already holds the head.
    `runs` is the head's `actions/runs` listing when the caller (run_status,
    via fetch_pr_rest) already read it; the merge gate passes none and keeps
    its own live read.
    """
    try:
        from fno.pr._proc import run
        from fno.pr._rest import _slug_or_reason, fetch_pr_info_rest

        slug, _why = _slug_or_reason(cwd)
        sha = str(sha or "").strip()
        if not sha and slug:
            info, _reason = fetch_pr_info_rest(str(pr_number), cwd=cwd, repo=slug)
            sha = str((info or {}).get("head_sha") or "").strip()
        if not sha or not slug:
            return dict(_NO_RECOVERY)

        def _get(path: str):
            res = run(["gh", "api", f"repos/{slug}{path}"], cwd=cwd)
            return json.loads(res.stdout) if res.ok else None

        rows = runs if isinstance(runs, list) else None
        if rows is None:
            rows = _get(f"/actions/runs?head_sha={sha}&per_page=100")
            if isinstance(rows, dict):
                rows = rows.get("workflow_runs")
        if not isinstance(rows, list):
            return dict(_NO_RECOVERY)

        def _attempts(run_id: str) -> list:
            data = _get(f"/actions/runs/{run_id}/attempts?per_page=100")
            if isinstance(data, dict):
                data = data.get("workflow_runs")
            return data if isinstance(data, list) else []

        def _failed_jobs(run_id: str, attempt: int) -> list:
            data = _get(f"/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100")
            jobs = data.get("jobs") if isinstance(data, dict) else data
            return [
                str(j["name"])
                for j in jobs or []
                if j.get("name")
                and str(j.get("conclusion") or "") in _RERUN_FAIL_CONCLUSIONS
            ]

        return _recovery_from_run_rows(rows[:_RERUN_MAX_RUNS], _attempts, _failed_jobs)
    except Exception:  # noqa: BLE001 - fail open: a fact, never a second red
        return dict(_NO_RECOVERY)


def rerun_recovery_note(payload: dict) -> None:
    """Print the rerun-recovery warning from a payload (payload-keyed so the
    cache serve replays it - docs/architecture/pr-status-verdict.md)."""
    if payload.get("rerun_recovered"):
        names = ", ".join(payload.get("recovered_failures") or ["unknown"])
        sys.stderr.write(
            "note: green on re-run; earlier failed attempt: " + names
            + ". A passing re-run is a recovery, not proof the defect is gone.\n"
        )


def coverage_recompute_note(coverage: dict) -> None:
    """Print the coverage recompute note on stderr (payload-keyed; shared
    with the cache serve - docs/architecture/pr-status-verdict.md)."""
    import sys

    note = coverage.get("recompute")
    if note and note != "recomputed":
        sys.stderr.write(f"note: coverage recompute: {note}\n")


def failures_note(payload: dict) -> None:
    """Print the per-check failure notes on stderr (payload-keyed; shared
    with the cache serve - docs/architecture/pr-status-verdict.md)."""
    import sys

    for f in payload.get("failures") or []:
        if not isinstance(f, dict):
            continue
        check = str(f.get("check") or "(unnamed check)")
        line = f"note: {check} failed"
        if f.get("step"):
            line += f" at step '{f['step']}'"
        if f.get("first_error"):
            line += f": {f['first_error']}"
        sys.stderr.write(line + "\n")
        if f.get("unreached_steps"):
            names = ", ".join(str(n) for n in f["unreached_steps"])
            sys.stderr.write(
                f"note: {check}: fail-fast never ran: {names}. An unreached step is not a pass.\n"
            )
        if f.get("detail"):
            sys.stderr.write(f"note: {check}: {f['detail']}\n")


def verdict_line(payload: dict) -> str:
    """One human line for the payload `fno do pr status` prints as JSON.

    Slot ordering, the four retired misreadings, and the payload-keyed
    contract are narrated in docs/architecture/pr-status-verdict.md.
    """
    checks = payload.get("checks") or {}
    unsettled = checks.get("unsettled")
    if payload.get("settled"):
        settled_slot = "settled"
    elif isinstance(unsettled, int):
        settled_slot = f"unsettled({unsettled})"
    else:
        settled_slot = "unsettled"
    # `_map_mergeable` turns GitHub's null (still computing) into the string
    # UNKNOWN before it reaches the payload; the parenthetical replaces the
    # reader's interpretation with the meaning, which was the fourth
    # misreading. An ABSENT value is a different fact: the error payload and
    # the terminal-PR arm never asked GitHub, so "not yet computed" would
    # claim a computation nothing is running.
    raw_mergeable = payload.get("mergeable")
    if raw_mergeable == "MERGEABLE":
        mergeable_slot = "mergeable"
    elif raw_mergeable == "CONFLICTING":
        mergeable_slot = "CONFLICTING"
    elif raw_mergeable == "UNKNOWN":
        mergeable_slot = "mergeable-unknown(not-yet-computed)"
    else:
        mergeable_slot = "mergeable-unavailable(no-answer)"
    head = str(payload.get("head") or "")
    coverage = payload.get("review_coverage") or {}
    cov_head = str(coverage.get("head_sha") or "")
    # Printed only on a mismatch: its presence is itself the signal, and a
    # matching coverage head is a fact nobody needs to check by eye.
    coverage_at = f" (coverage at {cov_head[:12]})" if cov_head and cov_head != head else ""
    blockers = [str(b) for b in (payload.get("ready_blockers") or [])]
    if payload.get("stale_reason"):
        # The stale serve rewrites `ready` to False without touching
        # `ready_blockers`, so without this the line would read NOT-ready
        # beside "no blockers" - a contradiction the clause exists to make
        # impossible.
        blockers.append(f"stale_serve: {payload['stale_reason']}")
    if payload.get("verdict") == "error" and payload.get("reason"):
        # The error payload carries no ready fields; the reason IS the
        # blocker, and a bare "no blockers" beside verdict error is the
        # same contradiction.
        blockers.append(f"error: {payload['reason']}")
    if blockers:
        clause = f"{len(blockers)} blockers: {', '.join(blockers)}"
    else:
        clause = "no blockers"
    missing = (payload.get("github_merge_state") or {}).get("missing_required_checks")
    clause += f" (missing: {', '.join(missing)})" if missing else ""
    # A red line names its first failing check and step, so the one-line read
    # already separates a pytest red from a lint red (the d-bdb035b6 incident:
    # two `smoke` reds fifteen minutes apart, unrelated remedies). The full
    # list rides in the JSON `failures` field and the stderr notes.
    failures = payload.get("failures") or []
    fail_slot = ""
    if isinstance(failures, list) and failures and isinstance(failures[0], dict):
        first = failures[0]
        label = str(first.get("check") or "?")
        if first.get("step"):
            label += f"[{first['step']}]"
        fail_slot = f" failing: {label}"
    history = payload.get("branch_history") or {}
    history_slot = f" history: {history['line']}" if history.get("line") else ""
    return (
        f"{payload.get('pr')} "
        f"{str(payload.get('pr_state') or 'UNKNOWN').upper()} "
        f"{payload.get('verdict')} "
        f"{settled_slot} "
        f"{mergeable_slot} "
        f"{'ready' if payload.get('ready') else 'NOT-ready'} "
        f"@ {head[:12] or 'unknown'}{coverage_at}{history_slot} - {clause}{fail_slot}"
    )


def _review_lane(pr: str, cwd: Optional[str]) -> bool:
    """Whether the merge gate's coverage guard engages for this PR.

    The SAME lane predicate ``fno do pr merge`` reads (``_review_lane_configured``
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


def _review_activity(branch: str, head: str, cwd: Optional[str]):
    """The in-flight-review reading, or a fail-closed stand-in.

    A review that is RUNNING is invisible to ``review_coverage``, which only
    knows what verdicts EXIST for a head. Three PRs read ``ready: true`` with an
    empty ``ready_blockers`` while a review of that exact head was still writing
    its fixes. Fail-closed on a raise, like every other hold read here: a guard
    that cannot answer must not answer "clear".

    An empty ``branch`` is the one clear case - without it there is no hold key
    and no worktree to match, and a degraded fetch that omitted ``headRefName``
    is a missing INPUT, not a failed probe.
    """
    from fno.pr._review_hold import ReviewActivity, review_activity

    if not branch:
        return ReviewActivity(
            False,
            "",
            "",
            None,
            {
                "probed": False,
                "path": None,
                "dirty": None,
                "head": None,
                "note": "no head branch on the PR read",
            },
        )
    try:
        return review_activity(branch, pr_head=head, repo=cwd or os.getcwd())
    except Exception as exc:  # noqa: BLE001
        from fno.pr import _review_hold

        return ReviewActivity(
            True,
            _review_hold.REVIEW_HOLD_UNREADABLE,
            f"review-activity read failed ({exc}); refusing to assume no review is running",
            None,
            {"probed": False, "path": None, "dirty": None, "head": None, "note": str(exc)},
        )


def _merge_decision(pr: str, repo: str, facts: dict) -> dict:
    """The one merge decision, as an authorized-merge preview."""
    from fno.rust_binary import verb_call

    try:
        receipt = verb_call(
            "authorized-merge", {"cwd": repo, "pr": int(pr), "effect": "preview", **facts},
            timeout=180,
        )
    except Exception as exc:  # noqa: BLE001 - a broken transport is not a verdict
        receipt = {"outcome": "unknown", "detail": f"{type(exc).__name__}: {exc}"}
    if not isinstance(receipt.get("blockers"), list):
        receipt["blockers"] = [{
            "code": "merge_decision_unknown", "class": "unknown",
            "detail": str(receipt.get("detail") or "authorized-merge receipt unreadable"),
        }]
    return receipt


def _github_merge_blockers(pr_json, rollup, cwd):
    """GitHub's mergeStateStatus as named ready blockers; None when unasked."""
    from fno.rust_binary import VerbUnavailable, verb_call
    if pr_json.get("mergeStateStatus") is None:
        return None
    try:
        op = {k: pr_json.get(k) for k in ("mergeStateStatus", "baseRefName", "pr", "mergeable")}
        op.update(op="status-merge-blocker", rollup=rollup, cwd=cwd)
        return verb_call("authorized-merge", op, timeout=120)
    except VerbUnavailable as exc:
        return {"blockers": ["github_merge_state_unknown"], "source": str(exc)}


def _branch_history(pr_json, rollup, cwd, prior):
    from fno.rust_binary import verb_call
    try:
        result = verb_call("authorized-merge", {"op": "status-branch-history", "cwd": cwd, "branch": pr_json.get("headRefName"), "rollup": rollup, "workflow_runs": pr_json.get("workflowRuns") or [], "prior": (prior or {}).get("branch_history")}, timeout=60)
        return result if result.get("line") else None
    except Exception:
        return None


def _review_owner_guidance(coverage: dict, worktree: dict) -> Optional[dict]:
    """Explain a counted local review whose author differs from this session."""
    from fno.pr._reviews import counted_freshness

    verdicts = coverage.get("verdicts")
    if not isinstance(verdicts, list):
        return None
    counted_other_session = any(
        isinstance(verdict, dict)
        and verdict.get("producer") == "local_attestation"
        and verdict.get("name") == "code-review"
        and verdict.get("verdict") == "reviewed"
        and counted_freshness(verdict.get("freshness"))
        and verdict.get("attestation_origin") == "other_session"
        for verdict in verdicts
    )
    if not counted_other_session:
        return None
    raw_event_owner = coverage.get("author_session_id")
    event_owner = (
        raw_event_owner
        if isinstance(raw_event_owner, str) and raw_event_owner
        else None
    )
    live_owner = worktree.get("harness_session_id")
    if event_owner and live_owner == event_owner:
        authority_note = (
            f"{worktree.get('authority_note') or 'target manifest'}; "
            "matches coverage event author"
        )
    elif event_owner:
        authority_note = (
            "coverage event author; current worktree manifest owner differs"
        )
    else:
        authority_note = (
            "coverage event lacks author_session_id; current manifest is not "
            "historical evidence"
        )
    return {
        "attestation_origin": "other_session",
        "counts": True,
        "harness_session_id": event_owner,
        "worktree_path": worktree.get("path"),
        "manifest_path": worktree.get("manifest_path"),
        "authority_note": authority_note,
    }


def _merge_authority(repo: str) -> dict:
    """The resolved merge-authority axes for this repo.

    Two keys, both fail-open to None on an unreadable settings load: a
    status receipt that cannot read config says so rather than asserting
    "disabled" - a guessed NO here is the direction a wedged fleet reads as
    a disarm, and a guessed YES is the dangerous one.
    """
    try:
        from fno.config import load_settings_for_repo

        am = load_settings_for_repo(Path(repo)).auto_merge
        enabled = bool(am.enabled)
        grant = str(am.grant or "none")
        return {
            "config_auto_merge_enabled": enabled,
            "grant": grant,
        }
    except Exception:  # noqa: BLE001 - an unreadable config is not a verdict
        return {
            "config_auto_merge_enabled": None,
            "grant": None,
        }


def _observer_health() -> dict:
    """The watcher's liveness, for an execution receipt that needs an executor.

    A durable grant is only worth anything while something ticks: with no
    live watcher a granted PR waits forever, and the receipt must say so
    (`observer_unavailable`) instead of reading as a working merge lane.
    Reads the same liveness report the doctor and the SessionStart hook use -
    one probe, never a second verdict implementation.
    """
    try:
        from fno.pr_watch._install import liveness_report_live

        report = liveness_report_live()
        verdict = str(report.get("verdict") or "unknown")
        available = verdict in ("healthy", "healthy-pending")
        return {
            "state": "observer_available" if available else "observer_unavailable",
            "detail": str(report.get("detail") or verdict),
            "repair": "" if available else str(
                report.get("fix") or "fno config set pr_watch.enabled true; "
                "then verify with fno do pr watch status"
            ),
        }
    except Exception as exc:  # noqa: BLE001 - liveness never crashes a receipt
        return {
            "state": "unknown",
            "detail": f"liveness probe failed: {type(exc).__name__}: {exc}",
            "repair": "verify with fno do pr watch status",
        }


def _merge_execution_projection(repo: str, pr: str) -> dict:
    """The durable-grant execution state, through the ONE resolver.

    Answers "if the worker parked, would the watcher merge this now": the
    newest recorded receipt, the node claim's liveness, and the standing
    config, fail-closed in the resolver's every arm. A projection only - it
    never widens the merge verb's own gates. When a recorded receipt exists
    the projection also names the observer's health, because a grant without
    a live watcher is the AC12-ERR shape: loud, with a repair, and merging
    nothing.
    """
    try:
        from fno.pr._merge_grant import resolve_durable_grant

        verdict = resolve_durable_grant(int(pr), repo)
        projection = verdict.as_projection()
    except Exception as exc:  # noqa: BLE001 - a receipt never lies by crashing
        return {
            "state": "unknown",
            "reason": f"durable-grant resolve failed: {type(exc).__name__}: {exc}",
            "node_id": None,
            "claim_state": None,
        }
    if verdict.state != "absent":
        projection["observer"] = _observer_health()
    return projection


def run_status(
    pr: str, cwd: Optional[str] = None, *, review_reader=None, prior: Optional[dict] = None
) -> int:
    """Print a one-line JSON verdict for PR `pr`; return the exit code.

    The exit code is always the CI verdict's code; review fields are additive
    and advisory; `ready` is the authorized-merge preview verdict with
    `ready_blockers` naming the gate codes that hold
    (docs/architecture/pr-status-verdict.md, `run_status`).
    `prior` is the same head's previous payload; detail and rerun facts are
    reused within one head only (docs, `Reuse across reads of one head`).
    """
    import sys

    prior_payload: dict = prior if isinstance(prior, dict) else {}
    # A job id is minted per attempt, so a known id is the same completed job.
    known: dict = {}
    for f in prior_payload.get("failures") or []:
        if isinstance(f, dict) and f.get("job_id"):
            known[str(f["job_id"])] = f

    pr_json, reason = _fetch(pr, cwd)
    if pr_json is None:
        error = {
            "pr": pr,
            "verdict": "error",
            "settled": False,
            "green": False,
            "reason": reason,
        }
        # The rate-limit class rides as a FIELD, never as prose for a sibling
        # module to substring-match: `_cache` arms the fleet backoff on this
        # value, and prose gating broke silently the day GitHub reworded the
        # refusal body (it contains no "secondary"; see `fno.pr._rest`).
        rate_limit_class = getattr(reason, "rate_limit_class", "")
        if rate_limit_class:
            error["rate_limit_class"] = rate_limit_class
        sys.stderr.write(verdict_line(error) + "\n")
        sys.stdout.write(json.dumps(error) + "\n")
        return 4

    rollup = pr_json.get("statusCheckRollup") or []
    generic_rollup = without_coverage_statuses(rollup)
    verdict, code, counts = verdict_for(generic_rollup)
    green = verdict == "green"

    # / d-bdb035b6: a red verdict must name WHICH check failed and, so
    # far as the job log tells, WHICH step and error - counts alone let a
    # reader generalize one `smoke` red onto an unrelated PR. Runs inside this
    # read (never on a second, per-watcher one), so the detail lands in the
    # row `cached_status` serves and the whole fleet shares one enrichment
    # per TTL. Additive: a detail failure degrades to counts, never to a
    # wrong verdict.
    failures = None
    if verdict == "red":
        from fno.pr._failures import collect_failures

        try:
            # Only SETTLED fails: a CANCELLED or STALE latest run is a
            # taken-away run, not a concluded failure - its instruction is
            # already the "push again or rerun" note, and a detail entry would
            # read as a diagnosed defect that does not exist.
            failing_rows = [
                c
                for c in _latest_per_name(generic_rollup)
                if _classify(c) == "fail" and _has_settled_marker(c)
            ]
            failures = collect_failures(failing_rows, cwd, known=known)
        except Exception:  # noqa: BLE001 - the verdict stays authoritative
            failures = None

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
        reviews: Any = {
            "optional_reviews": [],
            "optional_reviews_unresolved": 0,
            "optional_reviews_resolved_unchanged": 0,
        }
        unresolved: Any = 0
        resolved_unchanged: Any = 0
        coverage: Any = dict(_NOT_ASKED_COVERAGE)
        review_lane = False
        code_review_required = False
        hold_reason = None
        # Same exemption the coverage conjunct takes: the guard protects what
        # WOULD merge, and a merged or closed PR has no would-merge left.
        from fno.pr._review_hold import ReviewActivity

        activity: Any = ReviewActivity(
            False,
            "",
            "",
            None,
            {
                "probed": False,
                "path": None,
                "dirty": None,
                "head": None,
                "note": "not asked: PR is terminal",
            },
        )
    else:
        # Additive review signal: computed AFTER the authoritative CI
        # verdict so a slow/failed review read can never delay or corrupt it.
        # Any failure degrades to "unknown"/None and leaves the CI verdict +
        # exit code untouched.
        reader = review_reader or read_optional_review_state
        try:
            reviews = reader(pr, cwd)
        except Exception:
            reviews = {
                "optional_reviews": "unknown",
                "optional_reviews_unresolved": None,
                "optional_reviews_resolved_unchanged": None,
            }
        unresolved = reviews.get("optional_reviews_unresolved")
        resolved_unchanged = reviews.get("optional_reviews_resolved_unchanged")

        # coverage signal, same additive/fail-open discipline as the
        # optional review read above. Read from the review_coverage event so a
        # human and the loop see one number (Ownership: Rust computes, Python
        # reads). Recomputed once when no usable row exists, so a
        # human report and the merge gate act on the same number instead of
        # status saying "no coverage" for a PR merge would clear after one
        # recompute. The PR head rides in from _fetch: without it the verb
        # would pin the emitted row to the LOCAL checkout's HEAD, planting a
        # wrong-head row both gates then disagree on. The lane answer comes
        # BEFORE the coverage read, not beside it: on a no-lane repo the
        # read's recompute is a 120s subprocess that appends coverage rows
        # nobody acts on - the exact cost `fno do pr merge` skips on this same
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
                int(pr),
                cwd,
                head=pr_json.get("headRefOid"),
                recompute=review_lane,
                recompute_postureless=False,
            )
        except Exception:
            # The producer's own sentinel, not a copy of it: a second literal
            # here is a shape that drifts the moment a key is added on one
            # side only.
            coverage = dict(_UNKNOWN_COVERAGE)
        hold_reason = _merge_hold_reason(pr, cwd)
        activity = _review_activity(
            pr_json.get("headRefName") or "", pr_json.get("headRefOid") or "", cwd
        )

    github_merge = None if is_terminal else _github_merge_blockers(pr_json, rollup, cwd)
    history_needed = verdict == "red" and not is_terminal and any(
        str(_alt(check.get("conclusion"), check.get("state"), "")).upper()
        in {"CANCELLED", "TIMED_OUT"}
        for check in _latest_per_name(generic_rollup)
    )
    branch_history = _branch_history(pr_json, generic_rollup, cwd, prior_payload) if history_needed else None
    # Rerun recovery, probed on every green read of a live PR (fail-open).
    rerun: Optional[dict] = None
    if verdict == "green" and not is_terminal:
        head_sha = pr_json.get("headRefOid")
        prior_green = (
            prior_payload.get("verdict") == "green"
            and prior_payload.get("head") == head_sha
            and "rerun_recovered" in prior_payload
            and isinstance(prior_payload.get("checks"), dict)
            and prior_payload["checks"].get("total") == counts["total"]
        )
        if prior_green:
            rerun = {
                "recovered": bool(prior_payload.get("rerun_recovered")),
                "failed": list(prior_payload.get("recovered_failures") or []),
            }
        else:
            rerun = rerun_recovery(
                pr, cwd, sha=head_sha, runs=pr_json.get("workflowRuns")
            )
    rerun_fields = (
        {
            "rerun_recovered": bool(rerun.get("recovered")),
            "recovered_failures": list(rerun.get("failed") or []),
        }
        if rerun is not None
        else {}
    )
    # ONE merge decision: the probes this read already paid for
    # ride the ask, so the owner never spawns a second status read.
    receipt = _merge_decision(
        pr,
        cwd or os.getcwd(),
        {
            "verdict": verdict,
            "counts": counts,
            "rerun_recovered": bool(rerun.get("recovered")) if rerun is not None else None,
            "optional_reviews_unresolved": unresolved,
            "github_blockers": (github_merge or {}).get("blockers") or [],
            "covered_head": pr_json.get("headRefOid"),
        },
    )
    blocker_words = [str(b.get("code")) for b in receipt.get("blockers") or []]
    coverage_status_repost = None
    if not is_terminal and review_lane:
        from fno.pr import _reviews

        posted_states = {
            check.get("context"): str(check.get("state") or "").upper()
            for check in _latest_per_name(rollup)
            if check.get("context") in COVERAGE_STATUS_CONTEXTS
        }
        # The wanted states below read a KNOWN coverage word only; the
        # known_word guard on the trigger keeps that honest.
        required_state = (
            "FAILURE"
            if any(word.startswith("review_coverage_") for word in blocker_words)
            else "SUCCESS"
        )
        unavailable_state = "SUCCESS"
        wanted_states = {
            context: (
                required_state
                if context == _reviews.COVERAGE_STATUS_CONTEXT
                else unavailable_state
            )
            for context in COVERAGE_STATUS_CONTEXTS
        }
        # The publisher stamps the head the coverage row pins, so when that is
        # a DIFFERENT head than this one, comparing this head's posted states
        # can never converge - each republish lands on the pinned head and the
        # next run reads the same absent rows here. The moved head gets its
        # stamp from the refresher invalidate arm or the next review at it.
        covered_head = str(coverage.get("head_sha") or "")
        read_head = str(pr_json.get("headRefOid") or "")
        converges_here = not (covered_head and read_head and covered_head != read_head)
        # An unknown coverage word has no derivable wanted state: the publisher
        # answers a missing row REFUSED (posts FAILURE) and a dead head fetch
        # UNANSWERED (posts PENDING), and this read cannot tell which - a
        # guessed PENDING against a posted FAILURE republishes forever. The
        # stamp for an unknown read comes from the publisher's own arms.
        known_word = coverage.get("coverage") != "unknown"
        if known_word and converges_here and any(
            # Absent stays absent: the status read is deliberately not the
            # FIRST coverage-status writer (a PR no publisher has touched gets
            # its contexts from a publisher, not from a read). Only a POSTED
            # state that disagrees with the wanted one triggers the republish.
            posted_states.get(context) is not None
            and posted_states.get(context) != wanted
            for context, wanted in wanted_states.items()
        ):
            posted, note = _reviews.publish_coverage_status(
                int(pr), head=pr_json.get("headRefOid"), cwd=cwd
            )
            coverage_status_repost = "reposted" if posted else f"repost failed: {note}"
    owner_guidance = _review_owner_guidance(coverage, activity.worktree)
    payload = {
        "pr": pr,
        # The commit this verdict describes, so a caller can pin the
        # answer to a head instead of trusting it across a push.
        "head": pr_json.get("headRefOid"),
        "verdict": verdict,
        # total > 0 is load bearing: an empty rollup and an all-green
        # one both have zero unsettled entries, and only one of them
        # is decided. Existence must be stated, not inherited.
        # verdict != unknown too: a zero-real-check-run
        # rollup can have zero unsettled entries (every StatusContext
        # already settled) while still being an undecided read.
        "settled": verdict != "unknown" and counts["total"] > 0 and counts["unsettled"] == 0,
        "green": green,
        "pr_state": pr_json.get("state"),
        "mergeable": pr_json.get("mergeable"),
        "github_merge_state": github_merge,
        "checks": counts,
        **({"branch_history": branch_history} if branch_history else {}),
        # Red reads only: name the failing checks and, where the job log
        # reads, the failing step, its first error line, and the steps
        # fail-fast never reached (an unreached step is not a pass).
        **({"failures": failures} if failures is not None else {}),
        # Present iff the probe ran: absent and probed-false are not one fact.
        **rerun_fields,
        "optional_reviews": reviews.get("optional_reviews", "unknown"),
        "optional_reviews_unresolved": unresolved,
        "optional_reviews_resolved_unchanged": resolved_unchanged,
        "review_coverage": coverage,
        # The Rust posture verdict verbatim (never reclassified), so the
        # receipt answers "what review does this repo demand and is it met"
        # without reading config in the same turn. None when the row resolved
        # no rung - the same fact the merge gate refuses on.
        "review_posture": (
            coverage.get("review_posture")
            if isinstance(coverage.get("review_posture"), dict)
            else None
        ),
        # The merge-authority axes (AC7-HP): may the fleet merge, and via
        # which grant. Read from the same settings object the merge verb and
        # the config validator use, so a king asking "can this merge" gets the
        # authority from the receipt rather than from memory.
        "merge_authority": _merge_authority(cwd or os.getcwd()),
        # The execution axis (AC7-HP): whether a parked granted worker's PR
        # would be merged by the watcher right now - receipt, claim liveness,
        # and standing config through the one durable-grant resolver. None on
        # a terminal PR: nothing would execute, the same exemption the
        # coverage conjunct takes above.
        "merge_execution": (
            None
            if is_terminal
            else _merge_execution_projection(cwd or os.getcwd(), pr)
        ),
        # The round budget, from the review_coverage row the gate wrote at
        # this head - one producer per number (: a locally recomputed
        # floor read 3 on PR 1380 where the gate said 1). A row with no
        # rounds at this head answers null with a note naming the producer
        # to run, never a locally computed floor.
        **(
            {
                "rounds_used": coverage.get("rounds_used"),
                "max_rounds": coverage.get("rounds_max"),
                "rounds_exhausted": coverage.get("rounds_exhausted"),
            }
            if coverage.get("rounds_used") is not None
            else {
                "rounds_used": None,
                "max_rounds": None,
                "rounds_exhausted": None,
                "rounds_note": (
                    "no review_coverage row at this head; run fno-agents review-coverage"
                ),
            }
        ),
        # Reported whether or not it blocked. The original complaint was an
        # empty `ready_blockers` next to a live review: a reader has to be able
        # to see that both probes RAN and what each answered, because an absent
        # hold and an unasked question are not the same fact.
        "review_activity": {
            "blocker": activity.blocker,
            "detail": activity.detail,
            "hold": activity.hold,
            "worktree": activity.worktree,
        },
        "dispatch_hold": hold_reason,
        # The preview verdict.
        "merge_decision": receipt,
        "ready": not blocker_words,
        "ready_blockers": blocker_words,
    }
    if receipt.get("coverage_waiver") is not None:
        # The positive marker beside a waived ready: without it, a PR the
        # operator waived and a PR a reviewer covered render identically.
        payload["coverage_waiver"] = receipt["coverage_waiver"]
    if owner_guidance is not None:
        payload["review_owner_guidance"] = owner_guidance
    if coverage_status_repost is not None:
        payload["coverage_status_repost"] = coverage_status_repost
    # The human line precedes the JSON write on stderr: stdout is a machine
    # contract the fleet's watchers grep (`loopcheck.rs` prescribes
    # `2>/dev/null | grep '"settled": true'`), and stderr is already the
    # note channel this function uses below.
    sys.stderr.write(verdict_line(payload) + "\n")
    sys.stdout.write(json.dumps(payload) + "\n")
    rerun_recovery_note(payload)
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
        unsettled_now = [
            c for c in _latest_per_name(generic_rollup) if not _has_settled_marker(c)
        ]
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
    if isinstance(resolved_unchanged, int) and resolved_unchanged > 0:
        sys.stderr.write(
            f"note: {resolved_unchanged} optional review thread(s) resolved while the "
            "original diff line remains current; this does not block ready. Verify "
            "the explicit resolution rationale before merge.\n"
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
            passed_n = coverage.get("passed_count")
            if passed_n is not None:
                line += f", {passed_n} passed"
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
    if owner_guidance is not None:
        owner = owner_guidance.get("harness_session_id")
        worktree_path = owner_guidance.get("worktree_path")
        authority_note = owner_guidance.get("authority_note")
        if owner and worktree_path:
            sys.stderr.write(
                "note: attestation_origin other_session counts toward review coverage. "
                f"Self-attestation owner is {owner}; PR worktree is {worktree_path}.\n"
            )
        elif worktree_path:
            sys.stderr.write(
                "note: attestation_origin other_session counts toward review coverage. "
                f"Matched PR worktree {worktree_path}; harness_session_id unavailable: "
                f"{authority_note}.\n"
            )
        else:
            sys.stderr.write(
                "note: attestation_origin other_session counts toward review coverage. "
                f"PR worktree unavailable: {authority_note}; harness_session_id unavailable.\n"
            )
    if coverage.get("review_state") == "reviewer_refused":
        raw_verdicts = coverage.get("verdicts")
        verdicts = raw_verdicts if isinstance(raw_verdicts, list) else []
        refused = [
            verdict
            for verdict in verdicts
            if isinstance(verdict, dict) and verdict.get("verdict") == "refused"
        ]
        refused_names = [str(v.get("name")) for v in refused if v.get("name")]
        who = ", ".join(refused_names) or "configured reviewer"
        local_refused = [v for v in refused if v.get("producer") == "local_attestation"]
        if local_refused:
            # A local refused verdict is a review attempt that RAN and produced
            # no verdict. The refusal_reason names the class, and the two
            # classes carry different remedies: an empty diff means the
            # reviewer's checkout was the wrong one; an unresolvable base means
            # the checkout was fine but could not measure against the base.
            # Both classes can be present at once (two attempts), so each is
            # diagnosed with its own reviewer names - one message for all
            # classes misdiagnoses whichever it drops.
            empty = [
                str(v.get("name") or "review")
                for v in local_refused
                if str(v.get("refusal_reason") or "empty_diff") == "empty_diff"
            ]
            unresolvable = [
                str(v.get("name") or "review")
                for v in local_refused
                if str(v.get("refusal_reason") or "") == "unresolvable_base"
            ]
            if empty:
                sys.stderr.write(
                    f"note: reviewer_refused: {', '.join(empty)} ran and refused "
                    "to attest (an empty diff at the reviewer's checkout: it "
                    "sat on the base branch, so the review read nothing). Fire "
                    "from the PR worktree session (`fno do target "
                    "request-self-review --pr <n>`) or spawn the reviewer with "
                    "--cwd <worktree>.\n"
                )
            if unresolvable:
                sys.stderr.write(
                    f"note: reviewer_refused: {', '.join(unresolvable)} ran and "
                    "refused to attest (its checkout could not resolve the "
                    "base to a merge-base). Fetch the base in the reviewer's "
                    "checkout, or run a local review at HEAD.\n"
                )
        else:
            sys.stderr.write(
                f"note: reviewer_refused: {who} declined to review; run a local review at HEAD.\n"
            )
    # `unknown` from a degraded gh read and `unknown` from "nobody reviewed
    # this" are different facts; the recompute note is the only thing that
    # separates them, and the JSON field alone would never reach a terminal.
    coverage_recompute_note(coverage)
    failures_note(payload)
    return code


def main(argv: Sequence[str]) -> int:
    known = {"--refresh", "--no-cache"}
    flags = {str(a) for a in argv if str(a).startswith("-")}
    args = [a for a in argv if not str(a).startswith("-")]
    refresh = bool(flags & known)
    # An unrecognised flag is REFUSED, never dropped: `--refresh` exists for a
    # caller who distrusts a cached verdict, so silently ignoring `--refesh`
    # would hand back the very row they were trying to bypass. The split is on
    # ONE leading dash, not two: with `--` alone, `-x` fell into neither set
    # and was read as the PR number, which is the silent drop this refuses.
    # An EXTRA POSITIONAL is refused for the same reason an unknown flag is.
    # `main(["42", "43"])` answered for 42 and dropped 43 silently, which is
    # the same shape one line up: a caller asked something the parser did not
    # answer and got no signal. Typer rejects it today, and `main` is the
    # module entry that the next caller inherits.
    if len(args) != 1 or not flags <= known:
        import sys

        sys.stderr.write("usage: fno do pr status <pr-number> [--refresh|--no-cache]\n")
        return 2
    try:
        from fno.pr._cache import cached_status

        # The CLI chokepoint goes through the coalescing cache: N sessions
        # polling one PR issue one network read per TTL. The
        # library entry (run_status) stays uncached for programmatic callers
        # and tests.
        rc = cached_status(str(args[0]), refresh=refresh)
        # the call counter. No agent could see its own spend, so the
        # fleet's polls were invisible to the pollers. One stderr line on the
        # CLI path only - library callers read `_proc.GH_CALLS` directly.
        import sys

        from fno.pr import _proc, _quota

        sys.stderr.write(
            f"note: {_proc.GH_CALLS} gh call(s) this invocation{_quota.budget_note()}\n"
        )
        return rc
    except ToolMissing:
        import sys

        sys.stderr.write("fno do pr status: gh not found on PATH\n")
        return 127
