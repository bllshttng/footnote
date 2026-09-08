"""What identity gets stamped into an outgoing ``<fno_mail>`` envelope.
Lifted out of ``fno.agents.dispatch``, which re-exports both names."""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional


@dataclass(frozen=True)
class _MailCtx:
    """Sender identity stamped into the ``<fno_mail>`` envelope (node x-1f23)."""

    from_: str
    harness: str
    model: str
    node: Optional[str] = None
    to: Optional[str] = None
    from_session: Optional[str] = None
    # This message's own bus msg-id, rendered as the additive `id` attr on both
    # the live inject and the durable fallback, so a registered-agent send is
    # reply-correlatable and dedupable like the name lane. None on a relay hop,
    # which keeps that envelope byte-identical.
    id: Optional[str] = None
    origin: Optional[str] = None
    # The raw sender provider behind `harness`, which is the one-way wire
    # spelling, so a durable write reusing this ctx stamps provider_from with the
    # value the envelope was built from rather than resolving a second time.
    provider: Optional[str] = None
    # The RECIPIENT's full session id, rendering its own live crown into
    # the envelope trailer. Live delivery only; None omits the line.
    to_session: Optional[str] = None


def _build_mail_ctx(
    from_name: str,
    from_session: Optional[str],
    provider_from: Optional[str],
    to: Optional[str] = None,
    id: Optional[str] = None,
    origin: Optional[str] = None,
    to_session: Optional[str] = None,
) -> _MailCtx:
    """Build the ``<fno_mail>`` sender context from the dispatch provenance.

    ``origin`` is caller-stated, and an in-process caller never routes through
    the mail CLI's classify_origin, so the same floor binds here (d-02625dda):
    an ambient agent identity cannot declare an origin above peer. ``from`` is
    the sender's canonical session handle, or the bare ``from_name`` when the
    caller is unregistered. ``model`` is the invoking session's real model, from
    its own transcript store; an unresolvable one floors to ``"unknown"`` and is
    never fabricated. ``to`` and ``node`` are optional and omitted when None, and
    ``node`` stays None because dispatch has no truthful source for it today."""
    from fno.agents.self_stamp import resolve_self_model
    from fno.harness_identity import canonical_handle
    from fno.mail.envelope import harness_for_provider

    from_ = canonical_handle(from_session) if from_session else from_name
    from fno.decide import enforce_origin_floor

    return _MailCtx(
        origin=enforce_origin_floor(origin),
        from_=from_,
        harness=harness_for_provider(provider_from),
        model=resolve_self_model(),
        to=to or None,
        id=id or None,
        from_session=from_session,
        provider=provider_from,
        to_session=to_session or None,
    )
