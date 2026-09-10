"""Who sent this mail, and does an unresolvable sender stay silent?

Provenance is looked up by ``from_name``: a sender matching no registry row and
no ambient identity ships an envelope every reader renders as ``harness=
unknown`` with no ``from_session``. The lookup and the self-proof live here
with the check that says the miss out loud.
"""
from __future__ import annotations

import sys
from typing import TYPE_CHECKING, Optional

if TYPE_CHECKING:
    from fno.agents.registry import AgentEntry


def _resolve_sender_entry(
    entries: list[AgentEntry], from_name: str
) -> Optional[AgentEntry]:
    """Resolve a fresh-send sender through the spawn-written registry row.

    ``mail send`` passes the sender's canonical handle, while registry labels
    are friendly names. Resolve all supported address forms and floor misses,
    ambiguity, and legacy rows without a full session id to unproven values.
    """
    from fno.agents.registry import AgentResolutionError, resolve_agent_in

    try:
        return resolve_agent_in(entries, from_name).entry
    except AgentResolutionError:
        return None


def _proven_self_sender(from_name: str) -> tuple[Optional[str], Optional[str]]:
    """Proven sender identity when ``from_name`` is this session's own handle.

    The auto-stamp puts the caller's head-8 handle in ``from_name``. Under
    codex UUIDv7 that head is a truncated timestamp bucket, so a registered
    same-bucket sibling can be the UNIQUE registry hit for it and registry
    inference alone would stamp a stranger's full session id as
    ``from_session``. When the ambient identity proves this process owns the
    handle, its full id is already collision-free and wins - the same rule
    ``resolve_self_session_id`` documents for the envelope's ``from_session``.
    """
    from fno.agents.self_stamp import resolve_self_identity
    from fno.harness_identity import canonical_handle

    ident = resolve_self_identity()
    session_id = getattr(ident, "session_id", None)
    harness = getattr(ident, "harness", None)
    if session_id and harness and canonical_handle(session_id) == from_name:
        return harness, session_id
    return None, None


def _sender_provenance(
    sender: Optional[AgentEntry],
    from_name: str,
    self_proof: Optional[tuple[Optional[str], Optional[str]]] = None,
) -> tuple[Optional[str], Optional[str]]:
    self_harness, self_session = (
        self_proof if self_proof is not None else _proven_self_sender(from_name)
    )
    if self_session is not None:
        return self_harness, self_session
    if sender is None:
        return None, None
    return (
        getattr(sender, "harness", None),
        getattr(sender, "harness_session_id", None),
    )


def warn_sender_provenance_miss(
    from_name: str, provider_from: Optional[str], from_session: Optional[str]
) -> None:
    """Say it when sender provenance floors to nothing.

    A from_name matching no registry row and no ambient identity ships an
    envelope every reader renders as harness=unknown with no from_session.
    Delivery still proceeds - an unattended note must not die for lack of an
    attributable sender - but the miss is no longer silent.
    """
    if provider_from is not None or from_session is not None:
        return
    from fno.agents import events

    events.emit("sender_provenance_unknown", from_name=from_name)
    print(
        f"warning: sender {from_name!r} resolved to no registry row and no "
        "ambient identity; envelope provenance degrades to unknown",
        file=sys.stderr,
    )
