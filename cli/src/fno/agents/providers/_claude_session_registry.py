"""Read-only helpers over claude 2.1.143's session + jobs filesystem layout.

US2 (ab-8b90e793) reverse-engineered three external surfaces from the
``claude`` binary that have no documented schema. The provider adapter
imports them through this thin wrapper so a future replacement (MCP
channel server, daemon-backend wake) only needs to swap this module
rather than touching ``providers.claude``:

- ``~/.claude/sessions/<pid>.json`` — one entry per supervisor session.
  Function ``IE7`` in claude 2.1.143 owns this schema. The fields we
  use: ``messagingSocketPath`` (Unix socket; ``null`` when suspended),
  ``jobId`` (the 8-hex short-id matching ``backgrounded · <id> · …``),
  ``kind`` (``"bg"`` for background supervisor sessions; we ignore
  ``"interactive"``), ``sessionId``, ``cwd``.

- ``~/.claude/jobs/<short-id>/state.json`` — atomic-rename-written
  snapshot of the recipient session's current state. Fields used:
  ``state`` (``"running"`` | ``"done"`` | ``"completed"`` | ``"failed"``
  | ``"needs-input"`` | …), ``updatedAt`` (ISO string, the polling
  baseline), ``output.result`` (the reply text when terminal),
  ``intent``.

- ``~/.claude/jobs/<short-id>/timeline.jsonl`` — append-only event log.
  Each row carries ``at``, ``state``, ``detail``, ``text``. ``text`` is
  the surface output the recipient emitted; we concatenate ``text``
  from rows whose ``state`` is in the terminal-or-needs-input set when
  ``output.result`` is missing.

All paths derive from :func:`pathlib.Path.home`, so tests can pin
``HOME`` via :func:`monkeypatch.setenv` and avoid touching real claude
state.

This module is read-only: a future schema drift in claude is detected
either by ``locate_session`` returning ``None`` (jobId moved or kind
field disappeared) or by ``read_state_json`` raising ``JSONDecodeError``
after one retry. Either failure surfaces upward as an orphan or as a
SocketError in the provider adapter; we never silently misread.
"""
from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

# Terminal-or-needs-input states. The poll loop exits when state.json
# transitions to one of these; the timeline tail picks ``text`` from
# rows with these states (recipient may emit ``text`` during running
# states as tool-call narration, which we deliberately exclude).
TERMINAL_STATES = frozenset({"done", "completed", "failed", "needs-input"})
# Kept under the leading-underscore name for one release; future code
# imports the public ``TERMINAL_STATES`` from this module.
_TERMINAL_STATES = TERMINAL_STATES

# Backoff between the first and second read_state_json attempt. Short
# enough to be invisible in practice; long enough to clear the
# atomic-rename window observed in claude 2.1.143 (~1ms).
_RETRY_BACKOFF_SEC = 0.01


@dataclass(frozen=True)
class SessionLocator:
    """Pointer into the claude session registry for one bg supervisor session.

    ``messaging_socket_path`` is the Unix socket the BG8/CE7 protocol
    writes to. ``jobs_dir`` is the directory under
    ``~/.claude/jobs/<short-id>`` where ``state.json`` and
    ``timeline.jsonl`` live.
    """

    pid: int
    short_id: str
    messaging_socket_path: str
    jobs_dir: Path
    session_id: Optional[str] = None
    cwd: Optional[str] = None


@dataclass(frozen=True)
class StateSnapshot:
    """A parsed view of ``state.json`` carrying only the fields US2 reads."""

    state: str
    updated_at: Optional[str]
    output_result: Optional[str]
    intent: Optional[str] = None


def _sessions_dir() -> Path:
    """Return ``~/.claude/sessions`` resolved against the current HOME."""
    return Path.home() / ".claude" / "sessions"


def _jobs_dir_for(short_id: str) -> Path:
    """Return ``~/.claude/jobs/<short-id>`` resolved against the current HOME."""
    return Path.home() / ".claude" / "jobs" / short_id


def _daemon_dir() -> Path:
    """Return the claude daemon dir, honoring ``FNO_CLAUDE_DAEMON_DIR`` (tests)."""
    override = os.environ.get("FNO_CLAUDE_DAEMON_DIR")
    return Path(override) if override else Path.home() / ".claude" / "daemon"


def roster_live(short_id: str) -> bool:
    """True iff a session whose 8-hex short id matches is present in the daemon
    roster (``~/.claude/daemon/roster.json``).

    The roster keys workers by full ``sessionId``; the short id is its first
    hyphen segment (mirrors ``spawn_gate.census``). Lenient by design: a
    missing, torn, or type-drifted roster returns ``False`` and never raises --
    a strict read once zeroed the whole roster (``procStart`` drift). This is a
    cheap PRE-check for the control.sock ask fallback; the ``mail-inject`` verb's
    own connect is the authoritative liveness gate, so roster PRESENCE (not
    pid-liveness) is enough here."""
    try:
        raw = json.loads((_daemon_dir() / "roster.json").read_text(encoding="utf-8"))
    except (FileNotFoundError, OSError, ValueError, UnicodeDecodeError):
        return False
    workers = raw.get("workers") if isinstance(raw, dict) else None
    if not isinstance(workers, dict):
        return False
    for w in workers.values():
        if not isinstance(w, dict):
            continue
        sid = w.get("sessionId")
        if isinstance(sid, str) and sid and sid.split("-")[0] == short_id:
            return True
    return False


def roster_sessions() -> list[dict]:
    """Live claude sessions from the daemon roster, shaped for discover (x-605c).

    Each ``workers[*]`` row yields a discover-compatible dict
    ``{session_id, short_id, pid, cwd, status, agent}``. Lenient like
    :func:`roster_live`: a missing/torn/type-drifted roster yields ``[]`` and
    never raises. Roster PRESENCE surfaces the row -- the ``mail-inject`` connect
    is the authoritative liveness gate, so a stale row costs one failed inject and
    a durable floor, never a wrong delivery. A bg worker leaves no pid-sidecar, so
    this is the only source that surfaces it (the send-resolve bug this fixes)."""
    try:
        raw = json.loads((_daemon_dir() / "roster.json").read_text(encoding="utf-8"))
    except (FileNotFoundError, OSError, ValueError, UnicodeDecodeError):
        return []
    workers = raw.get("workers") if isinstance(raw, dict) else None
    if not isinstance(workers, dict):
        return []
    rows: list[dict] = []
    seen: set[str] = set()
    for w in workers.values():
        if not isinstance(w, dict):
            continue
        sid = w.get("sessionId")
        if not isinstance(sid, str) or not sid or sid in seen:
            continue
        seen.add(sid)
        try:
            pid = int(w.get("pid"))
        except (TypeError, ValueError):
            pid = 0
        rows.append(
            {
                "session_id": sid,
                "short_id": sid.split("-")[0],
                "pid": pid,
                "cwd": str(w.get("cwd") or ""),
                "status": None,
                "agent": "claude",
            }
        )
    return rows


def locate_session(short_id: str) -> Optional[SessionLocator]:
    """Find the bg session whose ``jobId`` matches ``short_id``.

    Returns a :class:`SessionLocator` when:
      - some ``~/.claude/sessions/<pid>.json`` has ``jobId == short_id``,
      - that entry has ``kind == "bg"``,
      - and ``messagingSocketPath`` is a non-empty string.

    Returns ``None`` when none of those conditions hold OR
    ``~/.claude/sessions`` does not exist (claude never ran). Corrupt
    JSON files in ``sessions/`` are skipped silently — a single junk
    file should not deny lookup of healthy entries.
    """
    sessions = _sessions_dir()
    if not sessions.exists():
        return None

    # Two-pass: collect all bg matches first, then prefer one with a
    # non-null socket. A supervisor respawn (claude auto-update, Domain
    # Pitfall 11) can leave the dead pid's session file behind with
    # messagingSocketPath=null AND the new pid's file with a live socket
    # sharing the same jobId — returning None on the first hit would
    # orphan the user even though the live session is reachable.
    null_socket_seen = False
    for entry_path in sorted(sessions.glob("*.json")):
        try:
            raw = json.loads(entry_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError, UnicodeDecodeError):
            continue
        if not isinstance(raw, dict):
            continue
        if raw.get("jobId") != short_id:
            continue
        if raw.get("kind") != "bg":
            continue
        sock = raw.get("messagingSocketPath")
        if not isinstance(sock, str) or not sock:
            null_socket_seen = True
            continue

        try:
            pid = int(entry_path.stem)
        except ValueError:
            continue

        return SessionLocator(
            pid=pid,
            short_id=short_id,
            messaging_socket_path=sock,
            jobs_dir=_jobs_dir_for(short_id),
            session_id=raw.get("sessionId"),
            cwd=raw.get("cwd"),
        )

    # Distinguishing socket-null from not-found at this layer would
    # require an extra return type; the caller's _classify_orphan_reason
    # re-walks the dir for the right discriminator. We keep this simple:
    # any non-success returns None, and null_socket_seen is informational
    # for callers that may want to skip the second walk.
    _ = null_socket_seen
    return None


def resolve_session_uuid(short_id: str) -> Optional[str]:
    """Resolve the FULL session UUID for a bg session by its 8-hex ``jobId``.

    The stream-json ``--resume`` lane keys on the full ``sessionId`` (the
    8-hex ``jobId``/``claude_short_id`` is only a 32-bit prefix, used by
    ``claude attach`` + the jobs-dir, not collision-proof as a resume key).

    Unlike :func:`locate_session`, this does NOT require a live
    ``messagingSocketPath``: an IDLE (socket-null) bg session is exactly the
    resume target, so the resolver reads ``sessionId`` regardless of socket
    state. It prefers a supervisor whose socket is live (the post-respawn
    file after a claude auto-update) but falls back to any ``kind == "bg"``
    match carrying a non-empty ``sessionId``, so a stale socket-null file
    left behind by a respawn still resolves.

    Returns ``None`` when no matching bg session file carries a non-empty
    ``sessionId``, or when ``~/.claude/sessions`` does not exist.
    """
    sessions = _sessions_dir()
    if not sessions.exists():
        return None

    fallback: Optional[str] = None
    for entry_path in sorted(sessions.glob("*.json")):
        try:
            raw = json.loads(entry_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError, UnicodeDecodeError):
            continue
        if not isinstance(raw, dict):
            continue
        if raw.get("jobId") != short_id or raw.get("kind") != "bg":
            continue
        sid = raw.get("sessionId")
        if not isinstance(sid, str) or not sid:
            continue
        sock = raw.get("messagingSocketPath")
        if isinstance(sock, str) and sock:
            return sid  # live supervisor wins outright
        if fallback is None:
            fallback = sid

    return fallback


def read_state_json(jobs_dir: Path) -> StateSnapshot:
    """Parse ``<jobs_dir>/state.json`` into a :class:`StateSnapshot`.

    Retries once on :class:`json.JSONDecodeError` after a short backoff
    to absorb the atomic-rename window that claude uses when updating
    state. If the second attempt also fails, the underlying
    ``JSONDecodeError`` propagates — the caller's poll loop treats that
    as a transient and tries again on the next cycle.
    """
    state_path = jobs_dir / "state.json"
    try:
        return _parse_state(state_path)
    except json.JSONDecodeError:
        time.sleep(_RETRY_BACKOFF_SEC)
        return _parse_state(state_path)


def _parse_state(state_path: Path) -> StateSnapshot:
    raw_text = state_path.read_text(encoding="utf-8")
    if not raw_text.strip():
        raise json.JSONDecodeError("empty", raw_text, 0)
    raw = json.loads(raw_text)
    if not isinstance(raw, dict):
        # Valid JSON but not an object (a list/primitive) is a malformed
        # snapshot: raise JSONDecodeError so every caller's existing
        # `except json.JSONDecodeError` degrades cleanly, instead of an
        # AttributeError on the `.get` below. Matches the Rust reader, whose
        # serde deserialize already rejects a non-object into a Parse error.
        raise json.JSONDecodeError("state.json is not a JSON object", raw_text, 0)
    output = raw.get("output") or {}
    return StateSnapshot(
        state=raw.get("state", ""),
        updated_at=raw.get("updatedAt"),
        output_result=output.get("result") if isinstance(output, dict) else None,
        intent=raw.get("intent"),
    )


def read_timeline_tail(jobs_dir: Path, offset: int) -> str:
    """Read ``<jobs_dir>/timeline.jsonl`` from ``offset`` and concatenate
    ``text`` fields from terminal-or-needs-input rows.

    The byte offset is the baseline captured before ``send_to_session``;
    everything appended since contributes to the tail. Lines whose
    ``state`` is outside :data:`_TERMINAL_STATES` are dropped (running
    rows describe in-flight tool calls, not the reply). Lines that
    fail to parse as JSON are skipped so a partial write at the tail
    of an append does not blow up the whole tail.

    Returns an empty string when ``timeline.jsonl`` does not exist
    (job hasn't emitted yet) or when no terminal-state rows appear
    after ``offset``.
    """
    timeline = jobs_dir / "timeline.jsonl"
    if not timeline.exists():
        return ""

    try:
        with open(timeline, "rb") as fh:
            fh.seek(offset)
            tail_bytes = fh.read()
    except OSError:
        return ""

    try:
        text = tail_bytes.decode("utf-8")
    except UnicodeDecodeError:
        return ""

    chunks: list[str] = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(row, dict):
            continue
        if row.get("state") not in _TERMINAL_STATES:
            continue
        piece = row.get("text")
        if isinstance(piece, str) and piece:
            chunks.append(piece)

    return "".join(chunks)
