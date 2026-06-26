"""Group 3 of the cross-session agent relay (x-908b / x-a2c9): the relay
ENVELOPE -- a thin view over the existing bus :class:`fno.bus.log.Envelope`,
plus the provenance WIRE FORMAT injected on every PTY hop.

Backing store is ``bus/`` only (Locked Decision #6 + the Group 3 plan section):
the relay does NOT invent a parallel store. A relay message IS a bus envelope
with ``kind == "relay"``; the relay-specific fields ride alongside the canonical
bus fields:

- ``msg_id``      -> bus ``id``           (idempotent dedup key).
- ``from``/``to`` -> bus ``from_``/``to`` (addresses; ``from_session`` is the id).
- ``hop_count`` + ``ttl`` -> bus ``meta`` (cycle termination; relay-private).
- ``provenance``  -> derived from ``from_session`` / ``provider_from`` /
  ``from_model`` and serialized to the ``<fno ...>`` wire tag below.

Provenance wire format (design "Provenance wire format", decided 2026-06-26)::

    <fno from="<session-id>" provider="<provider>" model="<model>"> <message>

A single-line attribute tag prefixing the (single-lined) body, NO closing tag:
the PTY Enter submits on newline so the turn boundary is the delimiter. Attributes
(not dash-delimited) because session-ids are dash-filled UUIDs. The sender
self-stamps -- the framed line is self-describing with no registry dependency.
This is the parse-safe successor to G1's prose ``_frame``.
"""
from __future__ import annotations

import re
from typing import Optional

from fno.bus.log import Envelope

# Relay-private meta keys on the bus envelope.
META_HOP = "hop_count"
META_TTL = "ttl"

# Default time-to-live (max relay hops before a cycle is cut). A small bound:
# real peer conversations are a handful of turns; anything past this is a loop.
DEFAULT_TTL = 8

RELAY_KIND = "relay"

# Parse the wire tag. ``model`` is optional (a peer may not always know it).
# DOTALL is deliberately NOT set: the tag and body are one physical line.
_TAG_RE = re.compile(
    r'^<fno\s+from="(?P<from_session>[^"]*)"\s+provider="(?P<provider>[^"]*)"'
    r'(?:\s+model="(?P<model>[^"]*)")?\s*>\s?(?P<body>.*)$'
)


def frame(from_session: str, provider: str, model: Optional[str], body: str) -> str:
    """Serialize one peer message to the ``<fno ...>`` wire line.

    The body is collapsed to a single line (Enter submits the TUI turn, so an
    embedded newline would submit early -- same constraint as G1's ``_frame``).
    """
    one_line = " ".join(body.split())
    model_attr = f' model="{model}"' if model else ""
    return f'<fno from="{from_session}" provider="{provider}"{model_attr}> {one_line}'


def parse(line: str) -> Optional[dict]:
    """Parse a wire line into ``{from_session, provider, model, body}``.

    Returns ``None`` if the line is not framed -- the caller uses that to refuse
    an unframed cross-provider injection (AC5-FR)."""
    m = _TAG_RE.match(line.strip())
    if not m:
        return None
    return {
        "from_session": m.group("from_session"),
        "provider": m.group("provider"),
        "model": m.group("model"),
        "body": m.group("body"),
    }


def is_framed(line: str) -> bool:
    """True if ``line`` carries a valid ``<fno ...>`` provenance tag."""
    return parse(line) is not None


def frame_envelope(env: Envelope) -> Optional[str]:
    """Frame a relay bus envelope for injection, or ``None`` if it cannot be
    framed (missing provenance -- no ``from_session`` or no ``provider_from``).

    A ``None`` return is the structural signal that the message is unframeable;
    the daemon refuses to inject it to a cross-provider recipient (AC5-FR)."""
    if not env.from_session or not env.provider_from:
        return None
    return frame(env.from_session, env.provider_from, env.from_model, env.body)


def hop_count(env: Envelope) -> int:
    """Read the relay hop count from the envelope meta (default 0)."""
    return _meta_int(env, META_HOP, 0)


def ttl(env: Envelope) -> int:
    """Read the relay ttl from the envelope meta (default :data:`DEFAULT_TTL`)."""
    return _meta_int(env, META_TTL, DEFAULT_TTL)


def _meta_int(env: Envelope, key: str, default: int) -> int:
    raw = (env.meta or {}).get(key, default)
    try:
        return int(raw)
    except (TypeError, ValueError):
        return default  # a junk meta value degrades to the default, never raises


def make_relay_envelope(
    *,
    from_session: str,
    to: str,
    body: str,
    provider_from: str,
    from_model: Optional[str] = None,
    hop_count: int = 0,
    ttl: int = DEFAULT_TTL,
    to_kind: str = "session",
    thread: Optional[str] = None,
    in_reply_to: Optional[str] = None,
) -> Envelope:
    """Build a ``kind="relay"`` bus envelope carrying the relay hop/ttl meta.

    ``from_`` is set to ``from_session`` so the address and the provenance id are
    the same handle (relay addresses are session ids)."""
    return Envelope.new(
        from_=from_session,
        to=to,
        kind=RELAY_KIND,
        body=body,
        provider_from=provider_from,
        from_session=from_session,
        from_model=from_model,
        to_kind=to_kind,
        thread=thread,
        in_reply_to=in_reply_to,
        meta={META_HOP: hop_count, META_TTL: ttl},
    )
