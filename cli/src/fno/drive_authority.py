"""Drive-authority detection (Phase 6 Wave 4, ab-8d258ddb).

``is_drive_authority_active()`` is the primitive every gate-hardening consumer
reads. While an operator holds an interactive / step / paranoid drive window on
any agent, the operator -- not the LLM -- authored the bytes flowing into that
agent's PTY. Gate signals observed during that window (a ``<promise>`` tag, a
gate-boolean edit) must therefore be treated as operator-initiated rather than
LLM authorship (LD3). Watch sessions are read-only and never open an authority
window (LD24/29); ``paranoid`` is a stricter ``step`` and hardens like it.

The detection is a read of each agent's ``state.json`` drive window, written by
the Rust daemon's drive handler. Liveness is the daemon's job: its heartbeat
watchdog evicts a stale driver (clearing the window) while it runs, and startup
recovery clears a leaked window after a daemon crash. So a window present here
is an authoritative "an operator is driving now" signal.

Stop-hook / PreToolUse consumption seam: call ``fno agents drive-authority``
(exit 0 when active) or import ``is_drive_authority_active``. The full
operator-authority matrix enforcement + its integration test land in Wave 8
(design Open Question #10); this module is the shared detection foundation.

Lives at the platform layer rather than under ``fno.agents``: it is a read-only
state-file reader whose only dependency is ``fno.paths``, and its consumers sit
below the runtime (``fno backlog done``, its deprecated spelling, the stop-hook seam).
"""
from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any, Optional

from fno import paths

#: Drive modes that open the gate-hardening window (LD24/29). ``watch`` is the
#: sole read-only carve-out and is intentionally absent.
AUTHORITY_MODES = frozenset({"interactive", "step", "paranoid"})


def _agents_dir(agents_dir: Optional[Path]) -> Path:
    """Resolve the agents root (``~/.fno/agents`` by default)."""
    if agents_dir is not None:
        return Path(agents_dir)
    return paths.state_dir() / "agents"


def active_drive_sessions(agents_dir: Optional[Path] = None) -> list[dict]:
    """Return one record per agent holding an authority drive window.

    Each record is ``{"short_id", "session_id", "mode"}``. Each agent's
    ``state.json`` is read best-effort: an unreadable, missing, or partial file
    is skipped (absence of a window is "no authority", never an error). Watch
    windows are excluded because their mode is not in :data:`AUTHORITY_MODES`.
    """
    base = _agents_dir(agents_dir)
    sessions: list[dict] = []
    if not base.is_dir():
        return sessions
    for child in sorted(base.iterdir()):
        if child.name.startswith(".") or not child.is_dir():
            continue
        try:
            data = json.loads((child / "state.json").read_text())
        except (OSError, ValueError):
            continue
        pty = data.get("pty")
        if not isinstance(pty, dict) or not pty.get("drive_active"):
            continue
        mode = pty.get("drive_mode")
        if mode not in AUTHORITY_MODES:
            continue
        sessions.append(
            {
                "short_id": data.get("short_id", child.name),
                "session_id": pty.get("drive_session_id"),
                "mode": mode,
            }
        )
    return sessions


def is_drive_authority_active(agents_dir: Optional[Path] = None) -> bool:
    """True iff any agent currently holds an interactive/step/paranoid window."""
    return bool(active_drive_sessions(agents_dir))


def _project_events_path() -> Path:
    """Resolve the project events.jsonl (best-effort, cwd fallback)."""
    try:
        from fno.paths import resolve_repo_root

        return resolve_repo_root() / ".fno" / "events.jsonl"
    except Exception:
        return Path(".fno") / "events.jsonl"


def emit_operator_initiated(
    action_type: str,
    *,
    source: str = "target",
    events_path: Optional[Path] = None,
    **data: Any,
) -> None:
    """Audit-tag an operator-initiated action taken during a drive window.

    The operator-authority matrix (design LD3/LD29) *allows* informational
    actions -- ``fno backlog done``, ``fno gate set``, artifact edits -- while
    an operator holds a drive window, but tags them so the audit trail
    attributes the action to the operator rather than the LLM. The generic
    ``operator_initiated`` event keeps the caller's action kind in
    ``data.action_type`` so every matrix action uses the validated canonical
    envelope and the same coordinated append path as other project events.

    Best-effort: callers gate on :func:`is_drive_authority_active` first, and
    any write failure here is swallowed so audit-tagging never breaks the
    primary command.
    """
    target = events_path if events_path is not None else _project_events_path()
    try:
        from fno.events import _build, append_event

        event = _build(
            "operator_initiated",
            source,
            {**data, "action_type": action_type},
        )
        append_event(event, events_path=target)
    except Exception as exc:
        print(
            f"drive_authority: warning: emit_operator_initiated({action_type!r}) "
            f"to {target}: {exc}",
            file=sys.stderr,
        )
