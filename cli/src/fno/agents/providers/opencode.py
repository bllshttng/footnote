"""fno.agents.providers.opencode - opencode session teardown.

opencode is a pane-hosted provider (``READABLE_PROVIDERS``): fno never
drives it through a Python ask adapter, so this module carries only what
``fno agents rm`` needs -- deleting a session record from opencode's own
store.

Verified against opencode v1.14.50, the same build the Rust reachability
probe (``crates/fno-agents/src/provider.rs``) was pinned to:

- ``opencode db`` opens the store READ-ONLY: a ``delete`` is rejected
  with "attempt to write a readonly database" (exit 1). It is a query
  surface only, which is why teardown goes through the supported
  ``opencode session delete <id>`` verb instead of raw SQL. That also
  keeps this module free of the store's sqlite layout mid-migration to
  v2, and inherits opencode's channel-aware database resolution.
- A missing session exits 1 with "Session not found: <id>" rather than
  succeeding, so callers wanting idempotent teardown must recognize it
  via :func:`looks_already_gone`.
- ``--pure`` runs without external plugins, which otherwise print
  loading banners to stdout ahead of real output.
"""
from __future__ import annotations

import re as _re
import subprocess

# Test seam, mirroring claude.py: patched by unit tests so no opencode
# binary is required.
_subprocess_run = subprocess.run

_SESSION_ID_RE = _re.compile(r"ses_[0-9A-Za-z]+\Z")


def is_session_id(value: str) -> bool:
    """True iff ``value`` is a well-formed opencode session id.

    ``ses_`` + ASCII alphanumerics. Mirrors the Rust probe's
    ``is_opencode_session_id`` so the two languages agree on what
    addresses an opencode session.
    """
    return bool(_SESSION_ID_RE.fullmatch(value or ""))


def looks_already_gone(output: str, session_id: str) -> bool:
    """True iff a failed delete failed only because the session was absent.

    opencode reports this as ``Error: Session not found: <id>``. Matching
    the message is unavoidable -- the exit code (1) is shared with real
    failures -- so it is kept to the stable half of the string and
    re-verified whenever the pinned opencode version moves.

    Anchored to the id so an unrelated failure that merely quotes the
    phrase cannot be misread as success.
    """
    return f"session not found: {session_id}".lower() in (output or "").lower()


def session_delete(session_id: str, *, timeout: float = 30.0) -> tuple[int, str]:
    """Run ``opencode --pure session delete <id>`` with a wall-clock timeout.

    Deletes the session RECORD. Transcript JSON under ``storage/session/``
    is opencode's to manage and is not touched by fno.

    Returns ``(exit_code, output)`` with stdout and stderr combined --
    the caller needs both to tell "already gone" from a real failure, and
    opencode splits its messages across the two. Non-zero exits do NOT
    raise, mirroring ``claude.claude_rm``, so the caller decides whether
    ``--force`` overrides.

    Raises:
        ValueError: id is not ``ses_``-shaped (never reaches the subprocess).
        FileNotFoundError: opencode not on PATH (caller maps to exit 14).
        subprocess.TimeoutExpired: wall-clock exceeded (caller maps to 15).
    """
    if not is_session_id(session_id):
        raise ValueError(
            f"refusing to delete opencode session: {session_id!r} is not a "
            "ses_-shaped session id"
        )
    result = _subprocess_run(
        ["opencode", "--pure", "session", "delete", session_id],
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return (result.returncode, (result.stdout or "") + (result.stderr or ""))
