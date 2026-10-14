"""Antigravity CLI (``agy``) conversation-id adapter.

``--conversation`` resumes by exact id; a typed id is VALIDATED here, and a
spawn that names none launches without one: the row binds the conversation
id at the agy session report. The print-turn mint is retired (a spawn no
longer pays a launch-time model call). Lane notes:
docs/architecture/thread-lanes.md."""
from __future__ import annotations

import re
from pathlib import Path

from fno.agents.dispatch import DispatchAskError

AGY_BINARY = "agy"
_UUID_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
    r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)


class AgySessionError(DispatchAskError):
    """agy returned or received no usable conversation id."""

    def __init__(self, message: str) -> None:
        super().__init__(message, exit_code=2)


def require_conversation_id(conversation_id: str) -> str:
    """Validate before any process launches on it: a truncated id names a
    DIFFERENT conversation to agy, never a resume."""
    text = (conversation_id or "").strip()
    if not _UUID_RE.match(text):
        raise AgySessionError(
            f"agy conversation id {conversation_id!r} is not a full UUID; "
            "agy resumes by exact id and a partial one silently opens a "
            "different conversation"
        )
    return text


def conversation_store_path(conversation_id: str) -> Path:
    """Where agy keeps this conversation - its OWN store, so a journey asserts
    the spawn's id against the harness, not against fno echoing itself."""
    store = Path.home() / ".gemini" / "antigravity-cli" / "conversations"
    return store / f"{require_conversation_id(conversation_id)}.db"
