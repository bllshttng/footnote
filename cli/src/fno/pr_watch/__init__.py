"""PR-state watcher: pure decision core.

This module exposes the ``decide()`` function and the ``Decision`` dataclass.
``decide()`` is intentionally pure (no I/O, no datetime.now() calls) so it
can be unit-tested exhaustively without stubbing. All inputs -- including the
current time and the merge-readiness verdict -- are passed in as parameters.

Decision precedence (first match wins):
    1. MERGED + not-yet-dispatched:
       - merge_ready True  -> Decision("merge", ...)
       - merge_ready False -> Decision("noop", reason="merge-not-ready")
    2. CLOSED (not merged) -> Decision("park", reason="closed")
    3. PR age > max_age_days -> Decision("park", reason="max-age")
    4. reviewers non-empty AND latest_review_ts strictly > watermark ->
       Decision("review", ...)
    5. else -> Decision("noop", ...)

Zero configured reviewers: step 4 is skipped entirely; merge-dispatch (step 1)
still works.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Literal, Optional

from fno.pr_watch._discover import PrObservation


DecisionKind = Literal["merge", "review", "noop", "park"]


@dataclass(frozen=True)
class Decision:
    """The watcher's verdict for one PR at one point in time.

    ``kind`` is the action to take:
    - ``"merge"``  -- fire /fno:pr merged headlessly for this PR.
    - ``"review"`` -- fire /fno:pr check headlessly (new reviewer activity).
    - ``"noop"``   -- no action needed; update the watermark timestamp only.
    - ``"park"``   -- the PR is closed or stale; remove it from the polling
                      set (task 1.2 will implement the parking-lot write).

    ``pr_number`` identifies which PR the decision is for.
    ``reason`` is a short human-readable string for logging.
    """

    kind: DecisionKind
    pr_number: int
    reason: str


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _iso_to_date_str(ts: Optional[str]) -> Optional[str]:
    """Return the date portion of an ISO-8601 string (first 10 chars).

    Used to compute day-level age without pulling in datetime parsing. For UTC
    timestamps the first 10 chars are always YYYY-MM-DD, so a lexical
    subtraction of total days is not needed -- we compare epoch day counts
    approximated from the date string.
    """
    if not ts:
        return None
    return ts[:10]


def _days_between(a_iso: str, b_iso: str) -> int:
    """Approximate number of days between two ISO-8601 timestamps.

    Converts both to Python ``date`` objects and returns the absolute delta.
    Only the date portion (first 10 chars) is used, so any timezone suffix is
    ignored -- this is safe for the max-age gate where precision to the day is
    sufficient.
    """
    from datetime import date

    def _parse(s: str) -> date:
        return date.fromisoformat(s[:10])

    return abs((_parse(a_iso) - _parse(b_iso)).days)


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------

def decide(
    obs: PrObservation,
    *,
    watermark: dict,
    reviewers: list[str],
    merge_ready: bool,
    now_iso: str,
    max_age_days: int = 14,
) -> Decision:
    """Return the watcher's dispatch verdict for one PR observation.

    Parameters
    ----------
    obs:
        The current observed state of the PR (produced by read_pr_state).
    watermark:
        The persisted per-PR state dict. Relevant keys:
        - ``"merge_dispatched"`` (bool): True when a merge event was already fired.
        - ``"last_review_ts"`` (str|None): ISO-8601 timestamp of the last
          review event we dispatched for; None means we have not dispatched
          any review yet (fire on any reviewer activity).
    reviewers:
        Configured reviewer logins. An empty list disables review-dispatch
        (step 4 is skipped) but merge-dispatch still works.
    merge_ready:
        Caller-supplied oracle: True when post_merge_readiness() passes for
        this PR's repo. Passed in so decide() stays pure and testable.
    now_iso:
        Current time as an ISO-8601 string (UTC). Injected so decide() has no
        datetime.now() call and tests can pin the clock.
    max_age_days:
        PRs older than this many days are parked (default 14).

    Returns
    -------
    Decision
        The first matching verdict from the precedence list in the module
        docstring. Never raises.
    """
    pr = obs.pr_number

    # 1. Merged
    if obs.state == "MERGED":
        if watermark.get("merge_dispatched"):
            return Decision("noop", pr, "merge-already-dispatched")
        if not merge_ready:
            return Decision("noop", pr, "merge-not-ready")
        return Decision("merge", pr, "pr-merged")

    # 2. Closed (not merged)
    if obs.state == "CLOSED":
        return Decision("park", pr, "closed")

    # 3. Max-age gate
    if obs.opened_at:
        age = _days_between(obs.opened_at, now_iso)
        if age > max_age_days:
            return Decision("park", pr, "max-age")

    # 4. New reviewer activity (only when reviewers are configured)
    if reviewers and obs.latest_review_ts is not None:
        last = watermark.get("last_review_ts")
        # A None watermark means we have never fired -- any activity triggers.
        if last is None or obs.latest_review_ts > last:
            return Decision("review", pr, "new-review-activity")

    # 5. Default: nothing to do
    return Decision("noop", pr, "no-new-activity")
