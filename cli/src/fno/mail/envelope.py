"""Thin adapter for Rust's ``<fno_mail>`` renderer and body guard."""

from __future__ import annotations

import json as _json
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


def _classify_in_rust(texts: list) -> list[dict]:
    """The ONE Python reach to the Rust mail-shape classifier: one subprocess
    for the whole batch (``fno-agents mail-envelope --classify``, stdin JSON
    array of texts). Every framing, id, guard and parse a Python reader needs
    lives in ``crates/fno-agents/src/mail_header.rs``; no Python module keeps
    a second shape test."""
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
    if isinstance(parsed, dict):
        return [parsed]
    if not isinstance(parsed, list) or len(parsed) != len(texts):
        raise MailShapeError(
            f"classifier returned {len(parsed) if isinstance(parsed, list) else type(parsed).__name__} "
            f"items for {len(texts)} texts"
        )
    return parsed


def mail_shape(texts: list) -> list[dict]:
    """Mail-shape facts per text: ``framing`` (``header`` | ``legacy_tag`` |
    ``cross_session`` | ``bare``), ``msg_id``, ``ids``, ``holds_tag``,
    ``envelope_block``, ``legacy_tags``, ``relay_parse``. Batch callers issue
    ONE call per read; single-text callers use the named helpers below."""
    return _classify_in_rust(texts)


def is_delivered(text: str) -> bool:
    """True when ``text`` IS a delivered turn: framed at the head (a header
    line, a legacy tag or a cross-session container). A turn that merely
    quotes one reads bare - prose."""
    return _classify_in_rust([text])[0]["framing"] != "bare"


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
    # An envelope always OPENS with its own attribution line: the delivered
    # header line (the one delivered shape), or the legacy `<fno_mail>` open
    # tag on a version-skewed binary. A successful render carrying neither is
    # a silent renderer; paste is the byte transport below this line, so fail
    # the send instead of typing an unattributed body. startswith, not a
    # substring hit: a lookalike like <fno_mailbox> appearing mid-text must
    # not pass.
    if not (rendered.startswith("<fno_mail") or rendered.startswith("`")):
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


# A bare substring match on "<fno_mail" also matches a prefix lookalike like
# "<fno_mailbox>" or "<fno_mailicious>", which cannot open a real envelope but
# still trips a refusal on ordinary text. The boundary rule lives in the Rust
# classifier (`mail_header::text_holds_legacy_tag`); this wrapper keeps the
# one check every forgery door calls.
def contains_fno_mail_tag(text: str) -> bool:
    """Case-insensitive: True if ``text`` contains an ``<fno_mail`` open tag or
    ``</fno_mail>`` close tag, through the Rust classifier.

    Case-insensitive because every reachable check (this one, the Rust
    injection guard, and the relay's single-line ``frame()``) keyed off an
    exact-case substring match, so a peer-controlled ``<FNO_MAIL ...>`` variant
    bypassed all of them at once (codex P1)."""
    return bool(_classify_in_rust([text])[0]["holds_tag"])


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
