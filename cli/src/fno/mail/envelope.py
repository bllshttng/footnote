"""The ``<fno_mail>`` agent-to-agent envelope renderer -- the SINGLE source for
the wire format G1. A Rust mirror of this renderer used to live in
``crates/fno-agents/src/claude_drive.rs``; node deleted it as dead code
once the live inject path moved to the bracketed-paste transport in
``mail_inject.rs``, which renders no envelope of its own and takes the already
-wrapped text this module produced. This module is now the sole renderer.

Rendered once here and shared by every live-delivery producer (node): the
claude ``control.sock`` inject (``fno-agents mail-inject``), the codex/gemini
daemon deliver, and the relay PTY hop (which uses the single-line transport
variant built from :func:`fno_mail_open`).

Field rule (from G1, reshaped in the v2 envelope): a field is a TAG attribute only if
the recipient needs it AT MESSAGE TIME and cannot cheaply look it up by
``from``. The envelope is the paired form ``open tag / body / close tag`` --
no footer lines of any kind. ``from`` holds the sender's FULL session id when
one is proven (the collision-safe reply address), falling back to the compact
handle. ``harness`` names the sender's harness; ``from_rank``/``to_rank`` name
live crowns read from the registry at render time; ``origin`` stays a machine
enum. Every attribute after ``from`` renders only when set.
"""
from __future__ import annotations

import re as _re
from functools import lru_cache
from pathlib import Path
from typing import Optional

from fno.agents.registry import TERMINAL_STATUSES, load_registry
from fno.paths import agents_registry_path

# Provider id -> the <fno_mail> ``harness`` vocabulary. The single mapping shared
# by the dispatch (live-inject) and relay (PTY hop) producers so the harness
# attribute reads the same everywhere.
_HARNESS_BY_PROVIDER = {"claude": "claude-code", "codex": "codex", "gemini": "gemini"}


def harness_for_provider(provider: Optional[str]) -> str:
    """Map a provider id to the ``<fno_mail>`` ``harness`` vocabulary (``claude``
    -> ``claude-code``; ``codex`` / ``gemini`` and unrecognized nonblank values
    unchanged). A missing or blank provider renders the explicit ``unknown``
    marker, never a vendor name: a null harness and a genuine claude harness
    must not be byte-identical on the wire, and this mapper never recovers a
    harness by inspecting the model or any other axis. The harness is legible
    context for how to reply, not unforgeable trust."""
    if not provider:
        return "unknown"
    return _HARNESS_BY_PROVIDER.get(provider, provider)


class ForgedEnvelopeError(ValueError):
    """A body or attribute attempted to smuggle an ``<fno_mail`` open or
    ``</fno_mail>`` close tag into a peer envelope."""


# A quote closes the attribute early; an angle bracket forges a tag boundary
# once the value is rendered inline. A handle, model id, node id, or msg id
# never legitimately needs either, so a sender who supplies one (e.g. an
# attacker-controlled `--from-name`) is refused rather than escaped: escaping
# would silently change the string a reader later matches against.
_UNSAFE_ATTR_CHARS = ('"', "<", ">")


def _refuse_unsafe_attr(name: str, value: str) -> None:
    if any(c in value for c in _UNSAFE_ATTR_CHARS):
        raise ForgedEnvelopeError(
            f"mail envelope attribute {name!r} contains a quote or angle "
            f"bracket ({value!r}); it could forge a second tag once rendered"
        )


def fno_mail_open(
    *,
    from_: str,
    harness: Optional[str] = None,
    from_rank: Optional[str] = None,
    to: Optional[str] = None,
    to_rank: Optional[str] = None,
    id: Optional[str] = None,
    reply_to: Optional[str] = None,
    node: Optional[str] = None,
    origin: Optional[str] = None,
) -> str:
    """Render the ``<fno_mail ...>`` OPEN tag with double-quoted attributes in
    Render order: ``from``, ``harness``, ``from_rank``, ``to``, ``to_rank``,
    ``id``, ``reply_to``, ``node``, ``origin``. Every attribute after ``from``
    renders only when set, and no reader assumes order (``reply_resolve.py``,
    ``drain_dedup.py`` and the Rust ``split`` reads are all order-free).

    ``from`` holds the sender's FULL session id when the caller resolved one
    (D2: one attribute, one address that cannot collide), falling back to the
    compact handle for an unregistered sender. ``harness`` is the raw harness
    name spelled through :func:`harness_for_provider` (``claude`` ->
    ``claude-code``). ``from_rank``/``to_rank`` are the two live crowns,
    computed by :func:`wrap_fno_mail` from the registry and passed here for
    rendering only.

    The relay PTY hop reuses this open tag for its single-line, no-close
    transport variant (the Enter newline is its delimiter).

    ``origin`` renders only when set and not ``peer``: both the Python and the
    Rust door treat an absent origin as peer, so the common case costs no
    attribute.

    Every attribute is validated here, the one chokepoint every caller of this
    renderer shares, so a caller composing the open tag straight from a
    peer-supplied ``--from-name`` cannot smuggle a second tag through it."""
    for name, value in (
        ("from", from_),
        ("harness", harness),
        ("from_rank", from_rank),
        ("to", to),
        ("to_rank", to_rank),
        ("node", node),
        ("id", id),
        ("reply_to", reply_to),
        ("origin", origin),
    ):
        if value:
            _refuse_unsafe_attr(name, value)
    if origin:
        from fno.decide import MAIL_ORIGINS

        if origin not in MAIL_ORIGINS:
            raise ForgedEnvelopeError(
                f"mail envelope origin {origin!r} is not one of {MAIL_ORIGINS}"
            )
    s = f'<fno_mail from="{from_}"'
    if harness:
        s += f' harness="{harness_for_provider(harness)}"'
    if from_rank:
        s += f' from_rank="{from_rank}"'
    if to:
        s += f' to="{to}"'
    if to_rank:
        s += f' to_rank="{to_rank}"'
    if id:
        s += f' id="{id}"'
    if reply_to:
        s += f' reply_to="{reply_to}"'
    if node:
        s += f' node="{node}"'
    if origin and origin != "peer":
        s += f' origin="{origin}"'
    return s + ">"


@lru_cache(maxsize=8)
def fleet_has_crown_at(registry_path: Path) -> bool:
    """Return whether the registry AT ``registry_path`` describes a crowned fleet.

    The path is an ARGUMENT, and it is the cache key. A zero-argument cached
    read is a global keyed on nothing: the first caller in a process fixes the
    answer for every caller after it, so the result depends on execution order
    rather than on the root the caller asked about (R5). This mirrors
    the Rust ``fleet_has_crown_at`` in ``crates/fno-agents/src/mail_inject.rs``,
    which has taken its path as an argument from the start.

    A registry read failure keeps the crown read enabled. The extra attribute
    is cheap. Suppressing it when the fleet may be crowned is not.
    """
    try:
        registry = load_registry(path=registry_path)
        return any(getattr(entry, "crown_label", None) is not None for entry in registry)
    except Exception:  # noqa: BLE001 - unreadable authority state fails safe
        return True


def fleet_has_crown() -> bool:
    """``fleet_has_crown_at`` against the registry THIS side writes.

    ``agents_registry_path()``, not ``agents_home_dir()/registry.json``. They
    coincide by default and are two knobs, which the registry's own bump
    refusal names: "config.paths.agents_registry_path, or FNO_AGENTS_HOME for
    the Rust side". ``crown_level`` is stamped through ``update_registry`` ->
    ``_registry_path`` -> ``agents_registry_path()`` (``registry.py:885``), so
    reading the Rust home means reading a file this side never wrote once the
    two are configured apart. A missing file is not an error -- ``load_registry``
    returns ``[]`` -- so the fail-safe below never fires and the rank is
    dropped on a genuinely crowned fleet. One resolver per state file, and it
    is the writer's (R4).
    """
    return fleet_has_crown_at(agents_registry_path())


# Deliberately UNCACHED: a cache keyed on the path survives a succession, so a
# long-lived renderer would keep naming the deposed holder. One read per message.
def crown_at(registry_path: Path, session: Optional[str]) -> Optional[str]:
    """Return the live row's crown label for ``session``, or ``None``.

    One read, both directions: the sender's ``from_rank`` and the recipient's
    ``to_rank`` ask one registry one question, so they cannot drift into two
    rules. A read failure returns no crown, because unreadable state must never
    manufacture standing."""
    if not session:
        return None
    try:
        registry = load_registry(path=registry_path)
        row = next(
            (
                entry
                for entry in registry
                if session in {entry.harness_session_id, entry.related_session_id}
                and entry.status not in TERMINAL_STATUSES
            ),
            None,
        )
        return getattr(row, "crown_label", None) if row is not None else None
    except Exception:  # noqa: BLE001 - unreadable authority state grants nothing
        return None


def _to_rank(to_session: Optional[str]) -> Optional[str]:
    """The recipient's ``to_rank`` attribute value, or ``None`` when nothing
    honest can be said. Gated on ``to_session`` FIRST: ``none`` is a positive
    claim about the reader's authority, and an unresolved address is an absence
    rather than a reading. An unreadable registry is that same absence and needs
    its own probe, because ``fleet_has_crown`` fails OPEN while ``crown_at``
    fails CLOSED and the two alone would tell a live king it had been deposed."""
    if not to_session or not fleet_has_crown():
        return None
    path = agents_registry_path()
    crown = crown_at(path, to_session)
    if crown is not None:
        return crown
    try:
        load_registry(path=path)
    except Exception:  # noqa: BLE001 - an unread registry is not a reading
        return None
    return "none"


# A bare substring match on "<fno_mail" also matches a prefix lookalike like
# "<fno_mailbox>" or "<fno_mailicious>", which cannot open a real envelope but
# still trips a refusal on ordinary text. `\b` requires a real word boundary
# right after "mail" (a space, `>`, or end of input all qualify; "b" in
# "mailbox" does not), mirroring the Rust `opens_envelope_tag` predicate and
# this module's own `_FNO_MAIL_OPEN_FRAGMENT_DELIM` in `fno.annotate.core`.
_FNO_MAIL_OPEN_TAG_RE = _re.compile(r"<fno_mail\b", _re.IGNORECASE)


def contains_fno_mail_tag(text: str) -> bool:
    """Case-insensitive: True if ``text`` contains an ``<fno_mail`` open tag or
    ``</fno_mail>`` close tag.

    Case-insensitive because every reachable check (this one, the Rust
    injection guard, and the relay's single-line ``frame()``) keyed off an
    exact-case substring match, so a peer-controlled ``<FNO_MAIL ...>`` variant
    bypassed all of them at once (codex P1)."""
    if _FNO_MAIL_OPEN_TAG_RE.search(text):
        return True
    return "</fno_mail>" in text.lower()


def refuse_if_forged(body: str) -> None:
    """Raise :class:`ForgedEnvelopeError` if ``body`` contains an ``<fno_mail``
    open tag or ``</fno_mail>`` close tag.

    Called from :func:`wrap_fno_mail` itself, not only from the CLI entry
    points that compose a body from user input: a relay-loop continuation
    (``_wrap_relay_body`` in ``fno.agents.dispatch``) and other producers call
    the renderer directly, bypassing any check that lives only at the CLI
    boundary. Putting the check in the shared renderer means every caller gets
    it for free."""
    if contains_fno_mail_tag(body):
        raise ForgedEnvelopeError(
            "mail body contains an <fno_mail> tag. The envelope frames peer "
            "mail; a body cannot contain one."
        )


def wrap_fno_mail(
    body: str,
    *,
    from_: str,
    node: Optional[str] = None,
    to: Optional[str] = None,
    id: Optional[str] = None,
    reply_to: Optional[str] = None,
    from_session: Optional[str] = None,
    origin: Optional[str] = None,
    to_session: Optional[str] = None,
    harness: Optional[str] = None,
) -> str:
    """Wrap ``body`` in the PAIRED ``<fno_mail>`` envelope::

        <fno_mail ...>
        {body}
        </fno_mail>

    Three lines, no footer lines of any kind (only fno writes the
    tag, so the tag itself marks agent text, and the crowns ride the header
    where ``from_rank`` is verified by the Rust door). ``from_session`` is the
    SENDER's full session id: when it resolves, it IS the ``from`` value and
    the ``from_rank`` read keys on it. ``to_session`` is the RECIPIENT's full
    session id, when a delivery lane resolved one; it renders ``to_rank``.

    ``harness`` is the raw sender harness (``claude``, ``codex``); it renders
    through :func:`harness_for_provider`. An unresolvable harness is omitted,
    never spelled ``cli`` or ``unknown`` on the wire.

    This is the form injected over the ``control.sock`` (claude) and stored in
    the durable bus body, so a delivered message is self-recording -- ``grep
    <fno_mail>`` across transcripts reconstructs the a2a history.

    Raises :class:`ForgedEnvelopeError` if ``body`` contains an ``<fno_mail``
    or ``</fno_mail>`` tag; see :func:`refuse_if_forged`."""
    refuse_if_forged(body)
    open_tag = fno_mail_open(
        from_=from_session or from_,
        harness=harness,
        from_rank=crown_at(agents_registry_path(), from_session),
        to=to,
        to_rank=_to_rank(to_session),
        id=id,
        reply_to=reply_to,
        node=node,
        origin=origin,
    )
    return "\n".join([open_tag, body, "</fno_mail>"])
