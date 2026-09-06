"""What a durable mail receipt says about the recipient's reachability.

Extracted from ``fno.mail.cli`` (file-budget: that module is shrink-only) so
the live-miss receipt logic has a home named by the question it answers.
"""

from __future__ import annotations


def _live_miss_age_suffix(recipient: str) -> str:
    """The transcript-age suffix a bare live-miss receipt carries (x-6d89 AC8).

    A bare live-miss reads the same for a transient miss to a genuinely live
    peer (re-send works) and for a session that stood down hours ago (nothing
    will drain it); the age is the discriminator. An unreadable transcript
    prints unknown, never 0s. Three lanes emit that receipt (the name lane,
    the job lane, the registered-agent lane), so the suffix lives here once.
    """
    from fno.agents.session_truth import resolve_session_truth
    from fno.agents.top import _fmt_age

    age_s = resolve_session_truth(recipient).get("last_activity_age_s")
    if age_s is None:
        return ", transcript age unknown"
    return f", transcript quiet {_fmt_age(age_s)}"
