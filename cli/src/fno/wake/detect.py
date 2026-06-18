"""Active-session detection for the wake-signal daemon.

Used by the launchd daemon (Phase 5) to decide whether to drain headlessly
or to drop a wake-signal for an active session to consume."""
from __future__ import annotations

import time
from enum import Enum
from pathlib import Path


class SessionState(Enum):
    IDLE = "idle"
    TARGET_ACTIVE = "target_active"
    INTERACTIVE_ACTIVE = "interactive_active"


DEFAULT_MAX_AGE_SECONDS = 300  # 5 minutes


def _file_recent(path: Path, max_age_seconds: int) -> bool:
    try:
        mtime = path.stat().st_mtime
    except FileNotFoundError:
        return False
    return (time.time() - mtime) < max_age_seconds


def _target_state_recent(repo_root: Path, max_age_seconds: int) -> bool:
    state = repo_root / ".fno" / "target-state.md"
    if not _file_recent(state, max_age_seconds):
        return False
    try:
        text = state.read_text(encoding="utf-8")
    except OSError:
        return False
    return "status: IN_PROGRESS" in text


def _claude_transcript_recent(repo_root: Path, max_age_seconds: int) -> bool:
    home = Path.home()
    encoded = str(repo_root.resolve()).replace("/", "-").lstrip("-")
    transcript_dir = home / ".claude" / "projects" / encoded
    if not transcript_dir.is_dir():
        return False
    for jsonl in transcript_dir.glob("*.jsonl"):
        if _file_recent(jsonl, max_age_seconds):
            return True
    return False


def detect_session_state(
    repo_root: Path,
    max_age_seconds: int = DEFAULT_MAX_AGE_SECONDS,
) -> SessionState:
    """Return the highest-priority active state, or IDLE."""
    if _target_state_recent(repo_root, max_age_seconds):
        return SessionState.TARGET_ACTIVE
    if _claude_transcript_recent(repo_root, max_age_seconds):
        return SessionState.INTERACTIVE_ACTIVE
    return SessionState.IDLE
