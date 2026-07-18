"""The ``<fno_mail>`` agent-to-agent envelope renderer -- the SINGLE Python source
for the wire format G1 (x-26df) locked in
``crates/fno-agents/src/claude_drive.rs``.

Rendered once here and shared by every live-delivery producer (node x-1f23): the
claude ``control.sock`` inject (``fno-agents mail-inject``), the codex/gemini
daemon deliver, and the relay PTY hop (which uses the single-line transport
variant built from :func:`fno_mail_open`). ``test_envelope`` pins these to the
Rust ``wrap_fno_mail`` output so the two renderers never drift.

Field rule (from G1): a field is a TAG attribute only if the recipient needs it
AT MESSAGE TIME and cannot cheaply look it up by ``from``. Both ``from`` and
``to`` are canonical handles (``<harness>-<short>``) -- the addressable identity;
the registry stays keyed by ``from``, and everything else (cwd, pid, lineage)
lives there.
"""
from __future__ import annotations

from typing import Optional

# Provider id -> the <fno_mail> ``harness`` vocabulary. The single mapping shared
# by the dispatch (live-inject) and relay (PTY hop) producers so the harness
# attribute reads the same everywhere.
_HARNESS_BY_PROVIDER = {"claude": "claude-code", "codex": "codex", "gemini": "gemini"}


def harness_for_provider(provider: Optional[str]) -> str:
    """Map a provider id to the ``<fno_mail>`` ``harness`` vocabulary (``claude``
    -> ``claude-code``; ``codex`` / ``gemini`` unchanged). An unknown or missing
    provider defaults to ``claude-code`` (the dominant harness). The harness is
    legible context for how to reply, not unforgeable trust."""
    if not provider:
        return "claude-code"
    return _HARNESS_BY_PROVIDER.get(provider, provider)


def fno_mail_open(
    *,
    from_: str,
    harness: str,
    model: str,
    node: Optional[str] = None,
    to: Optional[str] = None,
    reply_to: Optional[str] = None,
) -> str:
    """Render the ``<fno_mail ...>`` OPEN tag with double-quoted attributes:
    ``<fno_mail from="..." harness="..." model="..."[ node="..."][ to="..."][ reply_to="..."]>``.

    Mirrors Rust ``fno_mail_open``. The relay PTY hop reuses this open tag for its
    single-line, no-close transport variant (the Enter newline is its delimiter).
    ``reply_to`` (the answered bus msg-id) is additive, last, and omitted when
    absent, so a plain send stays byte-identical."""
    s = f'<fno_mail from="{from_}" harness="{harness}" model="{model}"'
    if node:
        s += f' node="{node}"'
    if to:
        s += f' to="{to}"'
    if reply_to:
        s += f' reply_to="{reply_to}"'
    return s + ">"


def wrap_fno_mail(
    body: str,
    *,
    from_: str,
    harness: str,
    model: str,
    node: Optional[str] = None,
    to: Optional[str] = None,
    reply_to: Optional[str] = None,
) -> str:
    """Wrap ``body`` in the PAIRED ``<fno_mail>`` envelope::

        <fno_mail ...>
        {body}
        </fno_mail>

    Mirrors Rust ``wrap_fno_mail``. This is the form injected over the
    ``control.sock`` (claude) and stored in the durable bus body, so a delivered
    message is self-recording -- ``grep <fno_mail>`` across transcripts
    reconstructs the a2a history."""
    open_tag = fno_mail_open(
        from_=from_, harness=harness, model=model, node=node, to=to, reply_to=reply_to
    )
    return f"{open_tag}\n{body}\n</fno_mail>"
