"""When a ``spawn --resume`` revives an existing row instead of forking.

The revival candidate is the row found by name, or the row the resumed
uuid itself answers to: a session whose row answers to another name (an
adopted short-id-named row) must revive under the caller's explicit name,
or the row and the harness title disagree. Split from dispatch under the
file budget: the revival rule is one question.
"""
from __future__ import annotations

from typing import Optional


def is_revival(existing, provider: str, resume_session_id: Optional[str]) -> bool:
    """True iff spawning an existing row (found by name, or by the resumed
    uuid when the row answers to another name) with ``--resume`` is a revival,
    not a collision (Fix 3).

    Gated on: the spawn carries ``--resume``, both the spawn and the row are
    claude, the row's own recorded ``claude_session_uuid`` equals the ``--resume``
    target, and the row's supervisor is NOT live. Liveness is a reality probe
    (``session_is_live``), never the registry ``status`` field, so a row whose
    supervisor is actually alive can never be revived into a second writer on one
    transcript. Every other same-name case (live row, uuid mismatch, no
    ``--resume``) stays fail-closed. The uuid check runs before the (heavier)
    liveness probe so the common mismatch never pays for a socket connect.
    """
    if not resume_session_id or provider != "claude":
        return False
    if getattr(existing, "harness", None) != "claude":
        return False
    if getattr(existing, "harness_session_id", None) != resume_session_id:
        return False
    from fno.agents.harnesses import claude as claude_mod

    short_id = getattr(existing, "short_id", "") or None
    if short_id:
        # A liveness-probe error fails SAFE toward "possibly live": never revive
        # (--resume) into what could be a second writer on one transcript. A
        # spurious collision refusal is retryable; a double writer is not. So a
        # probe crash refuses the revival, it does not wave it through.
        try:
            if claude_mod.session_is_live(short_id):
                return False
        except Exception:
            return False
    return True


def session_revival(entries, provider: str, resume_session_id: Optional[str]):
    """The uuid-keyed revival candidate when the by-name lookup missed.

    ``None`` unless a row carrying the resumed uuid passes :func:`is_revival`.
    """
    if not resume_session_id:
        return None
    session_row = next(
        (
            e
            for e in entries
            if getattr(e, "harness_session_id", None) == resume_session_id
        ),
        None,
    )
    if session_row is not None and is_revival(session_row, provider, resume_session_id):
        return session_row
    return None


def revival_replacement(entries, entry, name: str, resume_session_id: Optional[str]) -> list:
    """One row per session id: the same-name revival replaces its own row;
    the short-id-named adopted row is replaced through the resumed uuid."""
    return [
        entry
        if (
            e.name == name
            or (
                resume_session_id
                and getattr(e, "harness_session_id", None) == resume_session_id
            )
        )
        else e
        for e in entries
    ]
