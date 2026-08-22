"""Provider-neutral classification for authored mail envelopes."""
from __future__ import annotations


AUTHORED_MAIL_KINDS = frozenset({"send", "heads-up", "question", "fyi"})


def is_authored_mail_kind(kind: str) -> bool:
    """Whether ``kind`` represents a message authored by a sender."""
    return kind in AUTHORED_MAIL_KINDS
