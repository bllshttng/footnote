"""Thin adapter for Rust's ``<fno_mail>`` renderer and body guard."""
from __future__ import annotations

import json as _json
import re as _re
from typing import Optional

from fno.paths import agents_registry_path

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


def _render_in_rust(payload: dict) -> str:
    from fno.rust_binary import find_dev_binary, resolve_binary
    import subprocess

    binary = find_dev_binary() or resolve_binary() or "fno-agents"
    result = subprocess.run(
        [str(binary), "mail-envelope", "--registry", str(agents_registry_path())],
        input=_json.dumps(payload),
        capture_output=True,
        text=True,
        timeout=5,
    )
    if result.returncode:
        raise ForgedEnvelopeError(result.stderr.strip())
    return result.stdout.rstrip("\n")


# A quote closes the attribute early; an angle bracket forges a tag boundary
# once the value is rendered inline. A handle, model id, node id, or msg id
# never legitimately needs either, so a sender who supplies one (e.g. an
# attacker-controlled `--from-name`) is refused rather than escaped: escaping
# would silently change the string a reader later matches against.
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
    """Render one open envelope tag through the Rust owner."""
    payload = locals().copy()
    payload["mode"], payload["from"] = "tag", payload.pop("from_")
    return _render_in_rust(payload)


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
    """Wrap body in the Rust envelope; session ids key live identity lookups."""
    payload = locals().copy()
    payload["mode"], payload["from"] = "wrap", payload.pop("from_")
    return _render_in_rust(payload)
