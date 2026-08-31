"""Shared termination-reason classification.

Single definition of "delivered" so the ledger writer and the scoreboard
cannot drift into two sets. DoneAwaitingMerge is deliberately absent: the
work is complete but the PR is not merged (human-gated), so counting it as
delivered would inflate delivery metrics before the work lands. DoneBatched
stays (it delivers via the shared batch PR).
"""

DELIVERED_TERMINALS = frozenset(
    {"DonePRGreen", "DoneAdvisory", "DoneDelivery", "DoneBatched"}
)
