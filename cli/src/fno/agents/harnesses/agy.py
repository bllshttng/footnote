"""Antigravity CLI (``agy``) conversation-id adapter.

agy takes no conversation id on the command line: ``--conversation`` resumes
one and nothing creates one. So the id comes from the only surface that
returns it, a print-mode turn whose JSON envelope carries ``conversation_id``.
That makes agy a callee-minted-read-back harness like cursor-agent, minus the
fd dance - ``agy -p`` prints and exits, where ``cursor-agent create-chat``
stays alive and has to be killed.

Measured 2026-09-03 on agy 1.1.24: the mint returned in 1.5s, the id equalled
the db filename under ``~/.gemini/antigravity-cli/conversations``, and a fresh
process given ``agy --conversation <id>`` painted the TUI with that
conversation's transcript restored.
"""
from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path

from fno.agents.dispatch import DispatchAskError

AGY_BINARY = "agy"
# The mint is a real turn, so it needs a real prompt. A no-op instruction keeps
# the conversation's first message harmless: the seed the spawn actually cares
# about arrives at the composer once the keeper has the TUI up.
MINT_PROMPT = "Reply with exactly: OK"
MINT_TIMEOUT_S = 120.0
_UUID_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
    r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)


class AgySessionError(DispatchAskError):
    """agy returned or received no usable conversation id."""

    def __init__(self, message: str) -> None:
        super().__init__(message, exit_code=2)


def require_conversation_id(conversation_id: str) -> str:
    """Validate an id before any process launches on it.

    A truncated or misspelled id is a DIFFERENT conversation to agy, never a
    resume: it would start an empty one under a name fno believes is occupied.
    """
    text = (conversation_id or "").strip()
    if not _UUID_RE.match(text):
        raise AgySessionError(
            f"agy conversation id {conversation_id!r} is not a full UUID; "
            "agy resumes by exact id and a partial one silently opens a "
            "different conversation"
        )
    return text


def create_conversation(cwd: Path | str, *, timeout_s: float = MINT_TIMEOUT_S) -> str:
    """Mint an agy conversation id by running one print-mode turn."""
    try:
        completed = subprocess.run(  # noqa: S603
            [
                AGY_BINARY,
                "-p",
                MINT_PROMPT,
                "--output-format",
                "json",
                "--dangerously-skip-permissions",
            ],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            timeout=timeout_s,
        )
    except OSError as exc:
        raise AgySessionError(f"agy could not start: {exc}") from exc
    except subprocess.TimeoutExpired as exc:
        raise AgySessionError(
            f"agy minted no conversation id within {timeout_s:g}s; an "
            "unauthenticated or wedged binary never prints its envelope"
        ) from exc

    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout or "").strip()
        raise AgySessionError(
            f"agy exited {completed.returncode} while minting a conversation "
            f"id: {detail or 'no output'}"
        )
    try:
        envelope = json.loads(completed.stdout)
    except ValueError as exc:
        raise AgySessionError(
            "agy print-mode output was not JSON, so no conversation id could "
            f"be read: {(completed.stdout or '').strip()[:400] or 'no output'}"
        ) from exc
    if not isinstance(envelope, dict) or "conversation_id" not in envelope:
        raise AgySessionError(
            "agy print-mode JSON carried no conversation_id key: "
            f"{sorted(envelope) if isinstance(envelope, dict) else type(envelope).__name__}"
        )
    return require_conversation_id(str(envelope["conversation_id"]))


def conversation_store_path(conversation_id: str) -> Path:
    """Where agy keeps the conversation named by ``conversation_id``.

    agy's own store, so a journey can assert the spawn's id against the
    harness rather than against fno's registry row echoing itself back.
    """
    return (
        Path.home()
        / ".gemini"
        / "antigravity-cli"
        / "conversations"
        / f"{require_conversation_id(conversation_id)}.db"
    )
