"""Thin adapter for Rust's ``<fno_mail>`` renderer and body guard."""

from __future__ import annotations

import json as _json
import os
import subprocess
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


class MailShapeError(RuntimeError):
    """The Rust mail-shape classifier failed or returned malformed output."""


def mail_shape(texts: list) -> list[dict]:
    """Per text: framing, msg_id, ids, holds_tag, envelope_block,
    legacy_tags, header_turns, relay_parse."""
    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary() or "fno-agents"
    try:
        result = subprocess.run(
            [str(binary), "mail-envelope", "--classify"],
            input=_json.dumps(texts),
            capture_output=True,
            text=True,
            timeout=5,
        )
    except subprocess.TimeoutExpired as exc:
        raise MailShapeError("mail-envelope --classify timed out after 5s") from exc
    if result.returncode:
        raise MailShapeError(result.stderr.strip() or "mail-envelope --classify failed")
    try:
        parsed = _json.loads(result.stdout)
    except _json.JSONDecodeError as exc:
        raise MailShapeError(f"malformed classifier output: {exc}") from None
    if not isinstance(parsed, list) or len(parsed) != len(texts):
        raise MailShapeError(f"classifier returned {len(parsed)} items for {len(texts)} texts")
    return parsed


def _render_in_rust(payload: dict) -> str:
    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary() or "fno-agents"
    try:
        result = subprocess.run(
            [str(binary), "mail-envelope", "--registry", str(agents_registry_path())],
            input=_json.dumps(payload),
            capture_output=True,
            text=True,
            timeout=5,
        )
    except subprocess.TimeoutExpired:
        # The renderer reads the registry under a shared flock; a sustained
        # writer leaves the send blocked with no envelope to paste.
        raise ForgedEnvelopeError(
            "mail-envelope render timed out after 5s (registry lock contention?); "
            "refusing to deliver a body without its attribution frame."
        ) from None
    if result.returncode:
        raise ForgedEnvelopeError(result.stderr.strip())
    rendered = result.stdout.removesuffix("\n")
    # A render must open with attribution. Held releases use a validated frame
    # whose following per-message headers preserve each original sender.
    held_release = payload.get("mode") == "held-release"
    if not (held_release or rendered.startswith("<fno_mail") or rendered.startswith("`")):
        raise ForgedEnvelopeError(
            f"mail-envelope render produced no envelope ({rendered[:80]!r}); "
            "refusing to deliver a body without its attribution frame."
        )
    return rendered


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


# The boundary rule lives in the Rust classifier; case-insensitive (codex P1).
def contains_fno_mail_tag(text: str) -> bool:
    """True if ``text`` holds an ``<fno_mail`` open or ``</fno_mail>`` close
    tag, through the Rust classifier."""
    low = text.lower()
    if "<fno_mail" not in low and "</fno_mail>" not in low:
        return False  # a real tag carries the literal; no subprocess needed
    return bool(mail_shape([text])[0]["holds_tag"])


def refuse_if_forged(body: str) -> None:
    """Raise :class:`ForgedEnvelopeError` when ``body`` holds an ``<fno_mail>``
    tag. Lives in the shared renderer so direct callers (relay continuations
    and other producers) get the guard too, not just CLI entry points."""
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
    held_release: bool = False,
) -> str:
    """Render a normal envelope or pass through a validated held-release turn."""
    payload = locals().copy()
    mode = "held-release" if payload.pop("held_release") else "wrap"
    payload["mode"], payload["from"] = mode, payload.pop("from_")
    if mode == "wrap":
        # The front's peeled --subject rides the render too, so the
        # delivered header's third field is the sender's subject, not the
        # body's first sentence. The peel already validated the shape.
        subject = (os.environ.get("FNO_MAIL_SUBJECT") or "").strip()
        if subject:
            payload["subject"] = subject
    return _render_in_rust(payload)
