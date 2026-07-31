"""Shared ambient harness session identity resolution."""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from typing import Mapping, Optional


# Highest precedence first. Callers that need ambiguity detection may inspect
# the same marker facts without duplicating names or harness mappings.
HARNESS_SESSION_MARKERS: tuple[tuple[str, str], ...] = (
    ("CODEX_THREAD_ID", "codex"),
    ("CLAUDE_CODE_SESSION_ID", "claude"),
    ("CODEX_SESSION_ID", "codex"),
    ("GEMINI_SESSION_ID", "gemini"),
    ("OPENCODE_SESSION_ID", "opencode"),
)

LEGACY_HARNESS_SESSION_MARKERS: tuple[tuple[str, str], ...] = (
    ("CLAUDE_SESSION_ID", "claude"),
)


# The mailbox handle is the random tail of the session id. The signature takes
# no harness ON PURPOSE: harness is an envelope attribute, never part of an
# address, and no code path may recover it from a handle string. A
# harness-prefixed address (`claude-<short8>`) is a
# retired form that is NOT accepted anywhere - a caller still producing one is a
# bug to fix at the source, so resolution refuses it by name rather than quietly
# translating it.
#
# This function is the Python source of the generated string: discovery,
# registration, receipts, send, and drain all call it. The Rust lifecycle client
# carries a parity-tested mirror because it cannot import Python. If those two
# rules differ, a durable send can address one handle while its recipient drains
# another and silently strand on the bus.
def canonical_handle(session_id: str) -> str:
    """The mailbox address: the final eight characters of the session id."""
    tail = session_id[-8:]
    return tail if session_id.startswith("ses_") else tail.lower()


def legacy_prefix_handle(session_id: str) -> str:
    """The retired first-eight address, for fail-closed lookup compatibility only."""
    return session_id[:8]


def claude_transport_short_id(session_id: str) -> str:
    """Claude's first-eight attach/job key, which is not a mailbox address."""
    return legacy_prefix_handle(session_id)


def session_handle_tier(token: str, session_id: str) -> Optional[int]:
    """Return full/canonical/legacy match tier (0/1/2), or ``None``.

    OpenCode identifiers are case-sensitive; UUID-family identifiers retain the
    historical case-insensitive paste behavior. Callers may prefer the explicit
    full-id tier, but must union canonical and legacy matches with every other
    short address category before deciding uniqueness.
    """
    token = (token or "").strip()
    if not token or not session_id:
        return None
    exact_case = session_id.startswith("ses_")

    def equal(value: str) -> bool:
        return token == value if exact_case else token.lower() == value.lower()
    for tier, value in enumerate(
        (session_id, canonical_handle(session_id), legacy_prefix_handle(session_id))
    ):
        if equal(value):
            return tier
    return None


# The retired harness-prefixed address. Kept ONLY so the send path can recognize
# one and refuse it with a message naming the fix, and so `fno doctor` can still
# report mail queued to one before the flip as the dead letter it is. Never an
# accepted address, never generated.
#
# Built from the harness map rather than a literal list: a hardcoded copy silently
# stops covering a harness the moment one is added, which is the same drift that
# produced the two-conventions mess this address change exists to end.
def _legacy_handle_re() -> "re.Pattern[str]":
    from fno.agents.harness_map import known_harnesses

    return re.compile(rf"^(?:{'|'.join(known_harnesses())})-[0-9a-fA-F]{{6,}}$")


LEGACY_HANDLE_RE = _legacy_handle_re()


def sync_harness_aliases(data: dict, legacy_session_keys: Mapping[str, str]) -> dict:
    """Two-way sync of ``harness_session_id`` with a store's legacy per-harness
    session-id key. The ONE source of the sync rule (x-ec59): the target manifest
    shim (``schemas/target.py``) and the agent-registry row coercion both call it,
    so canonical<->legacy resolution can never drift between the two.

    ``legacy_session_keys`` maps a harness name to that store's legacy session-id
    field, because the stores disagree on the claude key: the manifest uses
    ``claude_session_id``, the registry ``claude_session_uuid``.

    Rule (canonical wins): when ``harness_session_id`` is set it is authoritative
    and syncs the matching legacy key (a stale/conflicting legacy value is
    overwritten, never leaked); otherwise the first present non-null legacy value
    back-fills ``harness_session_id``. Mutates and returns ``data``. The harness
    <-> provider alias is store-specific and stays with each caller.
    """
    if not isinstance(data, dict):
        return data
    harness = str(data.get("harness") or "").lower()
    if data.get("harness_session_id"):
        legacy_key = legacy_session_keys.get(harness)
        if legacy_key:
            data[legacy_key] = data["harness_session_id"]
    else:
        # Adopt from THIS harness's own legacy key when the harness is known, so a
        # row carrying a stale legacy id of a DIFFERENT harness can't cross-
        # contaminate. Only a genuinely unknown/absent harness scans all keys (the
        # pre-migration row whose harness has not yet been resolved).
        if harness in legacy_session_keys:
            candidate_keys = [legacy_session_keys[harness]]
        else:
            candidate_keys = list(legacy_session_keys.values())
        for legacy_key in candidate_keys:
            value = data.get(legacy_key)
            if value and str(value).strip() and str(value).strip().lower() != "null":
                data["harness_session_id"] = value
                break
    return data


@dataclass(frozen=True)
class HarnessIdentity:
    """The resolved session id and its harness, or two ``None`` values."""

    session_id: Optional[str]
    harness: Optional[str]


def resolve_harness_identity(
    env: Optional[Mapping[str, str]] = None,
) -> HarnessIdentity:
    """Resolve the first nonblank ambient harness marker by shared precedence."""
    environ = os.environ if env is None else env
    for marker, harness in HARNESS_SESSION_MARKERS:
        session_id = (environ.get(marker) or "").strip()
        if session_id:
            return HarnessIdentity(session_id=session_id, harness=harness)
    return HarnessIdentity(session_id=None, harness=None)


def current_session_id(env: Optional[Mapping[str, str]] = None) -> Optional[str]:
    """Return the canonical ambient session id, with legacy Claude fallback."""
    environ = os.environ if env is None else env
    identity = resolve_harness_identity(environ)
    if identity.session_id:
        return identity.session_id
    for marker, _ in LEGACY_HARNESS_SESSION_MARKERS:
        session_id = (environ.get(marker) or "").strip()
        if session_id:
            return session_id
    return None


def current_session_ids(env: Optional[Mapping[str, str]] = None) -> set[str]:
    """Return every nonblank canonical or legacy ambient session id."""
    environ = os.environ if env is None else env
    return {
        session_id
        for marker, _ in (*HARNESS_SESSION_MARKERS, *LEGACY_HARNESS_SESSION_MARKERS)
        if (session_id := (environ.get(marker) or "").strip())
    }
