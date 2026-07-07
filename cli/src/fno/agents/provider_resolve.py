"""Shared provider/model resolution for dispatch verbs.

``resolve_dispatch_provider`` centralizes one precedence so every dispatch verb
defaults the provider the same way:

    explicit --provider  >  invoking-harness inference  >  builtin default (claude)

There is no ``config.agents.default_provider`` field today, so a config-default
rung would be a no-op; add it between inference and the builtin if that field
lands. Inference never guesses: an absent or ambiguous harness marker falls
through to the builtin default rather than picking a provider.
"""

from __future__ import annotations

import os
from typing import Mapping, Optional

# Env markers that identify the invoking harness, highest priority first. Mirrors
# _capture_parent_edge() in dispatch.py (the source_harness detector); kept as a
# small self-contained table here so the resolver carries no heavy import and
# stays env-injectable for tests. These are external tool env vars (stable).
_HARNESS_MARKERS: tuple[tuple[str, str], ...] = (
    ("CLAUDE_CODE_SESSION_ID", "claude"),
    ("CODEX_SESSION_ID", "codex"),
    ("GEMINI_SESSION_ID", "gemini"),
)

# decision_source vocabulary surfaced in the spawn receipt so a dispatch's
# provider choice is auditable after the fact. The resolver emits this subset.
PROVIDER_SOURCE_EXPLICIT = "explicit"
PROVIDER_SOURCE_HARNESS = "harness-inferred"
PROVIDER_SOURCE_BUILTIN = "builtin-default"


class DispatchFlagError(ValueError):
    """A dispatch flag value is invalid (empty --model or empty --provider)."""


def infer_invoking_harness(env: Optional[Mapping[str, str]] = None) -> Optional[str]:
    """Return the invoking harness name from env markers, or None if absent.

    Highest-priority marker wins (claude > codex > gemini), matching
    ``_capture_parent_edge``. A bg worker exposes ``CLAUDE_CODE_SESSION_ID`` too,
    so a dispatch running inside a claude session correctly infers ``claude``.
    """
    environ = os.environ if env is None else env
    for marker, harness in _HARNESS_MARKERS:
        if (environ.get(marker) or "").strip():
            return harness
    return None


def resolve_dispatch_provider(
    explicit: Optional[str], *, env: Optional[Mapping[str, str]] = None
) -> tuple[str, str]:
    """Resolve the dispatch provider and record how it was decided.

    Precedence: explicit flag > invoking-harness inference > builtin ``claude``.
    Returns ``(provider, decision_source)`` where decision_source is one of
    ``explicit`` / ``harness-inferred`` / ``builtin-default``.
    Raises :class:`DispatchFlagError` on an empty explicit provider. The
    provider-name set is NOT validated here: the downstream spawn path checks it
    substrate-aware (pane hosts the wider ``READABLE_PROVIDERS`` incl. agy/
    opencode; bg/headless the narrower dispatchable set), so a single set here
    would both duplicate that check and wrongly reject a pane-hostable provider.
    """
    if explicit is not None:
        provider = explicit.strip()
        if not provider:
            raise DispatchFlagError("--provider must not be empty")
        return provider, PROVIDER_SOURCE_EXPLICIT

    inferred = infer_invoking_harness(env)
    if inferred is not None:
        return inferred, PROVIDER_SOURCE_HARNESS
    return "claude", PROVIDER_SOURCE_BUILTIN


def reject_empty_model(model: Optional[str]) -> Optional[str]:
    """Validate a ``--model`` flag: None passes through; empty/whitespace rejected.

    Returns the model token unchanged when valid (Invariant: exact passthrough,
    no fuzzy resolution -- names with dots/colons/dashes survive verbatim).
    """
    if model is None:
        return None
    if not model.strip():
        raise DispatchFlagError("--model must not be empty")
    return model
