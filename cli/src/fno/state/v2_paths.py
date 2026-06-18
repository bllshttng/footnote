"""v1 / v2 state path resolution (phase 04).

The v2 spine lives under ``.fno/v2/`` so it can coexist with the
in-flight v1 state machine. An ``ab`` invocation with v2 enabled reads
v1's state for an active owner check and refuses to run when a live
v1 target is in progress - preventing the two state machines from
corrupting each other's gate artifacts and ledger entries.

Liveness is determined by the owner_pid in v1's frontmatter: if the
PID still responds to ``os.kill(pid, 0)`` the v1 session is "live"
and v2 refuses. An orphaned v1 state file (PID gone) does not block
v2 - the existing ``stale_owner_cleanup`` in the stop-hook archives
those files eventually.
"""

from __future__ import annotations

import errno
import os
from pathlib import Path
from typing import Any

import yaml


# --- Path helpers -------------------------------------------------------

def v2_root(repo_root: Path) -> Path:
    return repo_root / ".fno" / "v2"


def v2_state_path(repo_root: Path) -> Path:
    return v2_root(repo_root) / "target-state.md"


def v2_artifacts_dir(repo_root: Path) -> Path:
    return v2_root(repo_root) / "artifacts"


def v1_state_path(repo_root: Path) -> Path:
    return repo_root / ".fno" / "target-state.md"


# --- Frontmatter reader -------------------------------------------------

def _read_frontmatter(path: Path) -> dict[str, Any] | None:
    """Return the YAML frontmatter of ``path`` as a dict, or None.

    Returns ``None`` for any parse failure so callers can treat a
    malformed state file as "no active owner" rather than crashing.
    The stop-hook's ``stale_owner_cleanup`` owns recovery.
    """
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    if not text.startswith("---"):
        return None
    end = text.find("\n---", 4)
    if end == -1:
        return None
    block = text[3:end].strip("\n")
    try:
        data = yaml.safe_load(block)
    except yaml.YAMLError:
        return None
    return data if isinstance(data, dict) else None


# --- PID liveness -------------------------------------------------------

def _is_pid_alive(pid: int) -> bool:
    """Return True iff ``pid`` is a live process we can signal.

    Uses ``os.kill(pid, 0)``: succeeds when the process exists and we
    have permission to signal it. EPERM is treated as alive (process
    exists but it's another user's). ESRCH is dead. Anything else is
    treated as dead to keep detect_v1_conflict conservative.
    """
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError as exc:
        return exc.errno == errno.EPERM
    return True


# --- Conflict detection -------------------------------------------------

def detect_v1_conflict(repo_root: Path) -> str | None:
    """Return a human-readable reason if a live v1 session would conflict.

    A v1 session is "live" iff the state file exists AND its
    ``owner_pid`` still responds to signals. An orphaned state file
    (PID gone) is not a conflict - the stop-hook will archive it.

    Returns ``None`` when v2 is free to proceed.
    """
    state = v1_state_path(repo_root)
    data = _read_frontmatter(state)
    if data is None:
        return None

    pid = data.get("owner_pid")
    if not isinstance(pid, int):
        return None

    if not _is_pid_alive(pid):
        return None

    session_id = data.get("session_id", "<unknown>")
    status = data.get("status", "<unknown>")
    return (
        f"v1 target session is live: session_id={session_id!r}, PID={pid}, "
        f"status={status!r}. Let the v1 session finish or run "
        f"`rm {state}` after confirming the PID is actually gone."
    )


def ensure_v2_layout(repo_root: Path) -> None:
    """Create ``.fno/v2/`` and its artifacts/ subdirectory."""
    v2_artifacts_dir(repo_root).mkdir(parents=True, exist_ok=True)
