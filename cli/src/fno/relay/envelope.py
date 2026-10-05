"""Group 3 of the cross-session agent relay (/): the relay
ENVELOPE -- a thin view over the existing bus :class:`fno.bus.log.Envelope`,
plus the provenance WIRE FORMAT injected on every PTY hop.

Backing store is ``bus/`` only (Locked Decision #6 + the Group 3 plan section):
the relay does NOT invent a parallel store. A relay message IS a bus envelope
with ``kind == "relay"``; the relay-specific fields ride alongside the canonical
bus fields:

- ``msg_id``      -> bus ``id``           (idempotent dedup key).
- ``from``/``to`` -> bus ``from_``/``to`` (addresses; ``from_session`` is the id).
- ``hop_count`` + ``ttl`` -> bus ``meta`` (cycle termination; relay-private).
- ``provenance``  -> derived from ``from_session`` / ``from_harness`` /
  ``from_model`` and rendered into the delivered-header wire line below.

Provenance wire format: the relay rides the ONE delivered shape, the
delivered-mail header (:func:`fno.mail.envelope.wrap_fno_mail`) collapsed to a
single physical line with the transcript newline glyph::

    `@<sender> · <fmail-id> · <subject>` ⏎ <message>

The PTY Enter submits on newline so the turn boundary is the delimiter; the
glyph keeps the header framing on one line, and the Rust door reads it as the
header framing. The renderer refuses a body carrying its own tag or a
header-shaped line, and the sender self-stamps (an unknown session renders as
its own address, no registry dependency).
"""
from __future__ import annotations

from typing import Optional

from fno.bus.log import Envelope
from fno.inbox.store import generate_msg_id
from fno.mail.envelope import ForgedEnvelopeError, harness_for_provider, wrap_fno_mail

# Relay-private meta keys on the bus envelope.
META_HOP = "hop_count"
META_TTL = "ttl"

# Default time-to-live (max relay hops before a cycle is cut). A small bound:
# real peer conversations are a handful of turns; anything past this is a loop.
DEFAULT_TTL = 8

RELAY_KIND = "relay"

# The transcript one-line body separator (the Rust door's NEWLINE_GLYPH).
NEWLINE_GLYPH = " ⏎ "


def frame(from_session: str, body: str, harness: Optional[str] = None) -> str:
    """Serialize one peer message to the single-line delivered-header wire
    line (header, glyph, body). The body is collapsed to one line (Enter
    submits the TUI turn, so an embedded newline would submit early). Raises
    ForgedEnvelopeError on a body holding a tag or a header-shaped line (the
    Rust renderer refuses both), or a from_session that could forge the sender
    field. The daemon RPC and the mail-inject binary each take an
    already-framed string from this single producer."""
    for name, value in (("from_session", from_session), ("harness", harness)):
        if value is None:
            continue
        if '"' in value or ">" in value or "`" in value:
            raise ForgedEnvelopeError(
                f"relay {name} cannot hold a quote, angle bracket or backtick"
            )
    one_line = " ".join(body.split())
    rendered = wrap_fno_mail(
        one_line,
        from_=from_session,
        harness=None if harness is None else harness_for_provider(harness),
        id=generate_msg_id(),
    )
    return rendered.replace("\n", NEWLINE_GLYPH)


def parse(line: str) -> Optional[dict]:
    """Parse a legacy ``<fno_mail ...>`` wire line into ``{from_session,
    body}``; ``None`` when unframed or when the line rides the header form
    (the Rust door classifies that one directly)."""
    from fno.mail.envelope import mail_shape

    parsed = mail_shape([line])[0]["relay_parse"]
    return dict(parsed) if parsed else None


def is_framed(line: str) -> bool:
    """True if ``line`` carries a valid ``<fno_mail ...>`` provenance tag."""
    return parse(line) is not None


def frame_envelope(env: Envelope) -> Optional[str]:
    """Frame a relay bus envelope for injection, or ``None`` if it cannot be
    framed (missing provenance -- no ``from_session`` or no ``from_harness`` --
    or a forged body that :func:`frame` refused).

    A ``None`` return is the structural signal that the message is unframeable;
    the daemon refuses to deliver it through any vehicle rather than inject an
    unframed or forged body (AC5-FR)."""
    if not env.from_session or not env.from_harness:
        return None
    try:
        return frame(env.from_session, env.body, harness=env.from_harness)
    except ForgedEnvelopeError:
        return None


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
    from_harness: str,
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
        from_harness=from_harness,
        from_session=from_session,
        from_model=from_model,
        to_kind=to_kind,
        thread=thread,
        in_reply_to=in_reply_to,
        meta={META_HOP: hop_count, META_TTL: ttl},
    )
