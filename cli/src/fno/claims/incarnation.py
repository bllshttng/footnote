"""Incarnation fence (x-eea5 1.3).

A mail-wake fork and the supervisor-restarted original can both be alive on one
lineage. The one that LOST the ``session:<uuid>`` single-writer claim must refuse
outward actions (push, PR, merge) by construction - the 1de52b53 retro's rule.

The fence keys on the incarnation's OWN session uuid, which dissolves the need
for a separate lineage carrier: the restarted original's own uuid IS the root
(its ``session:<root>`` is held by the fork, so it is fenced); the fork's own uuid
is new (no claim there, so it proceeds - it is the claim holder of record). The
sole-incarnation case is invisible (no contender). Fail closed: an unreadable
claims dir refuses outward actions, because an unverifiable single-writer
guarantee IS the incident, not an inconvenience.
"""
from __future__ import annotations

import os
import re
import socket
from pathlib import Path
from typing import Optional, Tuple


def resolve_fence_session_uuid(cwd: Optional[Path] = None) -> Optional[str]:
    """The session uuid to fence on, from TARGET_SESSION_ID, the active target
    manifest's ``session_id``, or CLAUDE_CODE_SESSION_ID. None when no identity
    is resolvable (the fence is then a no-op - invisible)."""
    for env_var in ("TARGET_SESSION_ID", "CLAUDE_CODE_SESSION_ID"):
        val = os.environ.get(env_var)
        if val:
            return val
    manifest = (Path(cwd) if cwd else Path.cwd()) / ".fno" / "target-state.md"
    try:
        text = manifest.read_text(encoding="utf-8")
    except OSError:
        return None
    m = re.search(r"^session_id\s*:\s*(.+)$", text, re.MULTILINE)
    if m:
        val = m.group(1).strip().strip("\"'")
        return val or None
    return None


def _own_session_pid_host() -> Tuple[Optional[int], Optional[str]]:
    try:
        from .session_pid import resolve_session_pid

        pid = resolve_session_pid(from_pid=os.getpid())
    except Exception:  # noqa: BLE001 - uncapturable -> conservative (not provably ours)
        pid = None
    try:
        host = socket.gethostname()
    except OSError:
        host = None
    return pid, host


def incarnation_fence_blocks(
    session_uuid: Optional[str], *, claims_root: Optional[Path] = None
) -> Tuple[bool, str]:
    """Read-only: does THIS incarnation hold ``session:<session_uuid>``, or does
    another live incarnation? Returns ``(blocked, reason)``.

    - blocked=False: this session holds the claim, no claim exists (sole
      incarnation), or no uuid to fence on (invisible).
    - blocked=True: another live/suspect pid holds it, or the claims dir is
      unreadable (fail closed).
    """
    if not session_uuid:
        return False, ""
    from .core import claim_status
    from .io import claims_root_for

    key = f"session:{session_uuid}"
    try:
        info = claim_status(key, root=claims_root or claims_root_for(key))
    except Exception:  # noqa: BLE001 - unreadable single-writer state -> fail closed
        return True, f"incarnation-fence: claims directory unreadable for {key}"
    state = info.get("state")
    if state not in ("live", "suspect"):
        return False, ""  # free / stale / dead -> no live contender
    own_pid, own_host = _own_session_pid_host()
    if own_pid and info.get("pid") == own_pid and info.get("host") == own_host:
        return False, ""  # ours
    holder = info.get("holder", "?")
    pid = info.get("pid", "?")
    return True, (
        f"incarnation-fence: {key} held by {holder} (pid={pid}); "
        f"refusing outward action - another incarnation owns this lineage"
    )
