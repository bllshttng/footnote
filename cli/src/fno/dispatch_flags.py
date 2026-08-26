"""Shared ``--provider`` / ``--model`` flag resolution for dispatch verbs.

Lives at the platform layer, not under ``fno.agents``, because its callers span
every layer: the backlog CLI, target, mail, provenance and the agents CLI all
validate the same two flags. Parking it in the runtime package forced core
callers into an upward import for what is pure flag validation.

The residual upward edge this module used to carry is closed (x-cec8):
``fno.harness_identity`` built ``LEGACY_HANDLE_RE`` at import time from
``fno.agents.harness_map.known_harnesses()``, so importing it eagerly imported
``fno.agents``. The harness-name set now lives at this layer
(``fno.harness_names``), so ``fno.harness_identity`` builds the regex from L0
data with no runtime import; ``fno.harness_identity`` and ``fno.harness_names``
are both declared in the boundary map at this layer, so the (absent) edge is
visible to the check rather than hiding in an unmapped blind spot. The runtime
capability table (``fno.agents.harness_map``) asserts its keys stay in sync with
the name list, preserving the single-source-of-truth property.

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

from fno.harness_identity import resolve_harness_identity

# decision_source vocabulary surfaced in the spawn receipt so a dispatch's
# provider choice is auditable after the fact. The resolver emits this subset.
PROVIDER_SOURCE_EXPLICIT = "explicit"
PROVIDER_SOURCE_HARNESS = "harness-inferred"
PROVIDER_SOURCE_BUILTIN = "builtin-default"


class DispatchFlagError(ValueError):
    """A dispatch flag value is invalid (empty --model or empty --provider)."""


def infer_invoking_harness(env: Optional[Mapping[str, str]] = None) -> Optional[str]:
    """Return the invoking harness name from env markers, or None if unclear.

    Inference never guesses: it returns a harness only when markers identify
    exactly one distinct harness. Multiple markers for that same harness (for
    example Codex's thread id plus its legacy session id) agree; markers naming
    different harnesses remain ambiguous and fall through to None.
    """
    environ = os.environ if env is None else env
    return resolve_harness_identity(environ).harness


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
