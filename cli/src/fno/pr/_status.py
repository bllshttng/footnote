"""`fno pr status <n>` - one authoritative CI verdict for a PR (x-8b64 G).

Agents kept re-deriving CI-green from `statusCheckRollup` by hand (or trusting
`gh pr checks`, which disagrees with the rollup). This computes a single
settled/green/red verdict from `gh pr view --json statusCheckRollup`, handling
the in-progress case (a CheckRun with `status != COMPLETED` has an empty
`conclusion` and must read as *pending*, never red) and the no-checks case
(verdict `unknown`, never red).

Exit codes (so a caller can branch without re-parsing the JSON):
    0  green    - settled, every check passed
    1  red      - settled, at least one check failed
    2  pending  - not settled (a check still queued/running)
    3  unknown  - no checks on the PR
    4  error    - could not fetch PR state (no PR, gh error, bad JSON)
    127 gh missing
"""
from __future__ import annotations

import json
from typing import Any, Optional, Sequence

from fno.pr._proc import ToolMissing, run
from fno.pr._reviews import read_optional_review_state

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


def _fetch(pr: str, cwd: Optional[str]) -> Optional[dict]:
    res = run(
        ["gh", "pr", "view", pr, "--json", "state,statusCheckRollup"],
        cwd=cwd,
    )
    if not res.ok or not res.stdout.strip():
        return None
    try:
        return json.loads(res.stdout)
    except json.JSONDecodeError:
        return None


def verdict_for(rollup: Sequence[dict]) -> tuple[str, int, dict]:
    """Pure verdict computation. Returns (verdict, exit_code, counts)."""
    counts = {"total": len(rollup), "pass": 0, "fail": 0, "pending": 0}
    for c in rollup:
        counts[_classify(c)] += 1
    if not rollup:
        return ("unknown", 3, counts)
    if counts["fail"]:
        return ("red", 1, counts)
    if counts["pending"]:
        return ("pending", 2, counts)
    return ("green", 0, counts)


def run_status(pr: str, cwd: Optional[str] = None, *, review_reader=None) -> int:
    """Print a one-line JSON verdict for PR `pr`; return the exit code.

    The exit code is ALWAYS the CI verdict's code (0/1/2/3/4/127) - the review
    fields are additive and advisory (optional stays advisory; an unresolved
    optional finding on a green PR still exits 0). ``review_reader`` is injectable
    for tests; it defaults to the real time-boxed read.
    """
    import sys

    pr_json = _fetch(pr, cwd)
    if pr_json is None:
        sys.stdout.write(
            json.dumps({"pr": pr, "verdict": "error", "settled": False, "green": False})
            + "\n"
        )
        return 4

    rollup = pr_json.get("statusCheckRollup") or []
    verdict, code, counts = verdict_for(rollup)
    green = verdict == "green"

    # Additive review signal (x-705b): computed AFTER the authoritative CI verdict
    # so a slow/failed review read can never delay or corrupt it. Any failure
    # degrades to "unknown"/None and leaves the CI verdict + exit code untouched.
    reader = review_reader or read_optional_review_state
    try:
        reviews = reader(pr, cwd)
    except Exception:
        reviews = {"optional_reviews": "unknown", "optional_reviews_unresolved": None}
    unresolved = reviews.get("optional_reviews_unresolved")

    sys.stdout.write(
        json.dumps(
            {
                "pr": pr,
                "verdict": verdict,
                "settled": verdict in ("green", "red"),
                "green": green,
                "pr_state": pr_json.get("state"),
                "checks": counts,
                "optional_reviews": reviews.get("optional_reviews", "unknown"),
                "optional_reviews_unresolved": unresolved,
                # The obvious "read this, not green": ready iff CI is green AND no
                # optional finding is unresolved. Advisory - never the exit code.
                "ready": green and unresolved == 0,
            }
        )
        + "\n"
    )
    return code


def main(argv: Sequence[str]) -> int:
    if not argv:
        import sys

        sys.stderr.write("usage: fno pr status <pr-number>\n")
        return 2
    try:
        return run_status(str(argv[0]))
    except ToolMissing:
        import sys

        sys.stderr.write("fno pr status: gh not found on PATH\n")
        return 127
