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
)


# The mailbox handle is the bare first-8 of the session id - the same prefix that
# already keys resume/attach/peek/transcripts/registry, so a session has ONE
# identity everywhere. The signature takes no harness ON PURPOSE: harness is an
# envelope attribute, never part of an address, and no code path may recover it
# from a handle string. A harness-prefixed address (`claude-<short8>`) is a
# retired form that is NOT accepted anywhere - a caller still producing one is a
# bug to fix at the source, so resolution refuses it by name rather than quietly
# translating it.
#
# This ONE function is the single source of the generated string: the send-resolve
# path (discover), the registry row-name fallback, and the receive-side drain
# (mail drain-self) all call it. If any two computed it differently, a
# durably-queued message would address one handle while its recipient drained
# another and silently strand on the bus.
def canonical_handle(session_id: str) -> str:
    """The mailbox address: the bare first-8 of the session id."""
    return session_id[:8]


# The retired harness-prefixed address. Kept ONLY so the send path can recognize
# one and refuse it with a message naming the fix, and so `fno doctor` can still
# report mail queued to one before the flip as the dead letter it is. Never an
# accepted address, never generated.
LEGACY_HANDLE_RE = re.compile(r"^(?:claude|codex|gemini|opencode)-[0-9a-fA-F]{6,}$")


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
