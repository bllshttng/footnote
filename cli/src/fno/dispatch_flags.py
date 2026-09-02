"""Shared ``--harness`` / ``--model`` flag resolution for dispatch verbs.

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

``resolve_dispatch_harness`` centralizes one precedence so every dispatch verb
defaults the harness the same way:

    explicit --harness  >  invoking-harness inference  >  builtin default (claude)

Every rung of that chain is on the HARNESS axis: the middle rung reads harness
markers out of the environment, and the builtin ``claude`` is a CLI binary, not
a vendor. It resolved a "provider" until x-c55f, which is why an operator could
read a harness name out of a provider-shaped field and conclude a route had
fallen back. See ``docs/architecture/axis-vocabulary.md``.

There is no ``config.agents.default_harness`` field today, so a config-default
rung would be a no-op; add it between inference and the builtin if that field
lands. Note that ``config.agents.defaults.provider`` is NOT that field under
another name: the vocabulary doc's "Named exception" section keeps that key's
spelling while ruling that it carries harness values, and it loses to ``-H``
inside the spawn path rather than here.

Inference never guesses: an absent or ambiguous harness marker falls through to
the builtin default rather than picking one of the candidates it saw.
"""

from __future__ import annotations

import os
from typing import Mapping, Optional

from fno.harness_identity import resolve_harness_identity

# decision_source vocabulary surfaced in the spawn receipt so a dispatch's
# harness choice is auditable after the fact. The resolver emits this subset.
# The VALUES are a wire contract the receipt carries and stay verbatim; only
# the constant names moved onto the harness axis.
HARNESS_SOURCE_EXPLICIT = "explicit"
HARNESS_SOURCE_INFERRED = "harness-inferred"
HARNESS_SOURCE_BUILTIN = "builtin-default"


class DispatchFlagError(ValueError):
    """A dispatch flag value is invalid (empty --model or empty harness pin)."""


def infer_invoking_harness(env: Optional[Mapping[str, str]] = None) -> Optional[str]:
    """Return the invoking harness name from env markers, or None if unclear.

    Inference never guesses: it returns a harness only when markers identify
    exactly one distinct harness. Multiple markers for that same harness (for
    example Codex's thread id plus its legacy session id) agree; markers naming
    different harnesses remain ambiguous and fall through to None.
    """
    environ = os.environ if env is None else env
    return resolve_harness_identity(environ).harness


def resolve_dispatch_harness(
    explicit: Optional[str],
    *,
    env: Optional[Mapping[str, str]] = None,
    flag: str = "--harness",
) -> tuple[str, str]:
    """Resolve the dispatch harness and record how it was decided.

    Precedence: explicit flag > invoking-harness inference > builtin ``claude``.
    Returns ``(harness, decision_source)`` where decision_source is one of
    ``explicit`` / ``harness-inferred`` / ``builtin-default``.

    ``flag`` names the option the caller read ``explicit`` from, so the refusal
    sends the operator back to the flag they actually typed. It defaults to
    ``--harness`` because that is the axis this resolver owns; a caller whose
    own flag is spelled differently passes its own spelling.

    Raises :class:`DispatchFlagError` on an empty explicit value. The harness
    name set is NOT validated here: the downstream spawn path checks it
    substrate-aware (pane hosts the wider ``READABLE_PROVIDERS`` incl. agy/
    opencode; bg/headless the narrower dispatchable set), so a single set here
    would both duplicate that check and wrongly reject a pane-hostable harness.
    """
    if explicit is not None:
        harness = explicit.strip()
        if not harness:
            raise DispatchFlagError(f"{flag} must not be empty")
        return harness, HARNESS_SOURCE_EXPLICIT

    inferred = infer_invoking_harness(env)
    if inferred is not None:
        return inferred, HARNESS_SOURCE_INFERRED
    return "claude", HARNESS_SOURCE_BUILTIN


def configured_dispatch_harness(
    settings: object = None,
    *,
    verb: str = "target",
) -> tuple[Optional[str], Optional[str]]:
    """The config rung of the autonomous-dispatch harness axis, read once.

    Precedence inside the rung:

      ``agents.profiles.<verb>.provider``  the stage table; the home
      ``dispatch.harness``                 deprecated one release; fallback rung

    Both dispatch doors read the stage table for this axis: ``resolve_dispatch``
    (the seam behind dispatch-node.sh and ``fno agents dispatch resolve``) takes
    the returned value as its config rung, and ``fno agents spawn`` reads the
    same ``provider`` field through its own profile merge, so a caller cannot
    get two answers for one node.

    Returns ``(harness, shadowed_note)``. ``harness`` is None when neither key
    is set (the caller applies its own builtin). ``shadowed_note`` is set only
    when both keys were set and the stage table won by disagreement; it names
    the losing spelling for the resolve receipt's decision trail. Read-only and
    total: a missing block or field reads as unset, never raises.
    """
    if settings is None:
        try:
            from fno.config import load_settings

            settings = load_settings()
        except Exception:  # noqa: BLE001 - a bad config must not brick resolution
            return None, None
    profile_verb = (verb or "target").strip().lstrip("/")
    if profile_verb.startswith("fno:"):
        profile_verb = profile_verb[len("fno:"):] or "target"
    agents = getattr(settings, "agents", None)
    profiles = getattr(agents, "profiles", None) or {}
    profile = profiles.get(profile_verb) if profile_verb else None
    stage = (getattr(profile, "provider", "") or "").strip()
    legacy = (getattr(getattr(settings, "dispatch", None), "harness", "") or "").strip()
    if stage:
        if legacy and legacy != stage:
            note = (
                f"harness=deprecated dispatch.harness={legacy!r} ignored; "
                f"agents.profiles.{profile_verb}.provider={stage!r} is the home"
            )
            return stage, note
        return stage, None
    if legacy:
        return legacy, None
    return None, None


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
