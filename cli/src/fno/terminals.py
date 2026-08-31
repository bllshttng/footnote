"""Shared termination-reason classification.

Single definition of "delivered" so the ledger writer and the scoreboard
cannot drift into two sets. DoneAwaitingMerge is deliberately absent: the
work is complete but the PR is not merged (human-gated), so counting it as
delivered would inflate delivery metrics before the work lands. DoneBatched
stays (it delivers via the shared batch PR).

DoneUnreviewed is absent for the same reason, recorded here because its name
does not settle it the way DoneAwaitingReview and DonePlanned settle theirs:
``run_outcome`` sets ``awaiting_review_notify`` on it, so it means waiting,
not landed. Without this line the omission reads as an oversight and the next
reader adds it back.
"""

DELIVERED_TERMINALS = frozenset(
    {"DonePRGreen", "DoneAdvisory", "DoneDelivery", "DoneBatched"}
)
