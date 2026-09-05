"""What may a deferred-mail receipt claim about the recipient's liveness?

The live lane can defer with no failure cause. A receipt that then reads "is
not live" asserts something nobody measured, and it sends the reader to
resurrect a session that may be working fine. So this preamble repeats the
verdict of the one liveness oracle and names the evidence it rests on:
could-not-establish when the transcript cannot be read, and the transcript's
own state when it can. Only a transcript that positively reads stalled may
warn that the recipient may never drain its inbox.
"""

from fno.agents.session_truth import _humanize_age, resolve_session_truth

_TAIL = (
    "queued durably as recovery only - the recipient must drain its inbox "
    "to read this, and may never do so\n"
)


def deferred_liveness_head(target: str) -> str:
    """The receipt's first line: a transcript verdict, never a bare claim."""
    truth = resolve_session_truth(target)
    state = truth["state"]
    if state == "unknown":
        return f"mail: liveness of {target} could not be established; " + _TAIL
    age = _humanize_age(truth["last_activity_age_s"]).strip()
    if state == "stalled":
        return f"mail: {target} reads stalled on its transcript (silent {age}); " + _TAIL
    return (
        f"mail: {target} reads {state} on its transcript (activity {age} ago); " + _TAIL
    )
