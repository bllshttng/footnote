"""Shared ambient harness session identity resolution."""

from __future__ import annotations

import os
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


# The addressable cross-harness handle is ``<harness>-<first8>``. This ONE
# function is the single source of truth for that string: the send-resolve path
# (discover), the registry row name (register_existing_session), and the
# receive-side drain (mail drain-self) all call it. If any two computed it
# differently, a durably-queued message would address one handle while its
# recipient drained another and silently strand on the bus (the plan's one true
# silent failure). ``session_id[:8]`` matches the registry's historical slice so
# already-registered rows keep the same name.
def canonical_handle(harness: str, session_id: str) -> str:
    """The cross-harness address ``<harness>-<first8-of-session-id>``."""
    return f"{harness}-{session_id[:8]}"


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
