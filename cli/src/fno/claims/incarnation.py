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
from pathlib import Path
from typing import Optional, Tuple, Union

from .hostid import is_same_machine


def resolve_fence_session_uuid(cwd: Optional[Union[str, Path]] = None) -> Optional[str]:
    """The TRANSCRIPT uuid the single-writer ``session:<uuid>`` claim is held
    under - NOT the target run id.

    The claim is keyed on the harness/transcript uuid
    (``acquire_session_writer_claim`` uses the resumed transcript uuid).
    ``TARGET_SESSION_ID`` and the manifest's ``session_id`` field are the target
    RUN id, a different identity; fencing on them reads a nonexistent key as
    clear and silently fails to fence. Resolve the transcript uuid:
    ``CLAUDE_CODE_SESSION_ID``, then the manifest's ``harness_session_id``
    (canonical) or ``claude_session_id`` (legacy). None when no transcript
    identity is resolvable (the fence is then a no-op - invisible)."""
    val = os.environ.get("CLAUDE_CODE_SESSION_ID")
    if val:
        return val
    manifest = (Path(cwd) if cwd else Path.cwd()) / ".fno" / "target-state.md"
    try:
        text = manifest.read_text(encoding="utf-8")
    except OSError:
        return None
    for field in ("harness_session_id", "claude_session_id"):
        m = re.search(rf"^{field}\s*:\s*(.+)$", text, re.MULTILINE)
        if m:
            val = m.group(1).strip().strip("\"'")
            if val:
                return val
    return None


def _own_session_pid() -> Optional[int]:
    try:
        from .session_pid import resolve_session_pid

        return resolve_session_pid(from_pid=os.getpid())
    except Exception:  # noqa: BLE001 - uncapturable -> conservative (not provably ours)
        return None


def _describe_process(pid: object) -> str:
    """Name the process a blocking fence found: cmdline and uptime.

    Both operators in the 2026-08-21 specimens ran ps by hand to learn the
    blocker was a chat app; the gate that decides who may act must say so
    itself. An unreadable or absent process renders honestly - the state said
    live-or-suspect, and showing what the pid actually is (or that it cannot
    be inspected) beats failing open or closed on a probe error.
    """
    if not isinstance(pid, (int, str)) or isinstance(pid, bool):
        return "<uninspectable>"
    try:
        pid_int = int(pid)
    except (TypeError, ValueError):
        return "<uninspectable>"
    import time

    import psutil

    try:
        proc = psutil.Process(pid_int)
        cmdline = " ".join(proc.cmdline()) or proc.name()
        up_s = max(0, int(time.time() - proc.create_time()))
        return f"{cmdline[:160]}, up {up_s // 3600}h{(up_s % 3600) // 60}m"
    except psutil.NoSuchProcess:
        return "no such process"
    except Exception:  # noqa: BLE001 - AccessDenied and friends: name it, don't crash
        return "<uninspectable>"


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
    if state == "corrupted":
        # claim_status reports a malformed claim file as state="corrupted" without
        # raising; an unverifiable single-writer state fails closed, matching the
        # unreadable-dir arm below.
        return True, f"incarnation-fence: {key} claim file corrupted (unverifiable single-writer state)"
    if state not in ("live", "suspect"):
        return False, ""  # free / stale / dead -> no live contender
    own_pid = _own_session_pid()
    same_machine = is_same_machine(info.get("host"), info.get("machine_id"))
    if own_pid and info.get("pid") == own_pid and same_machine:
        return False, ""  # ours
    holder = info.get("holder", "?")
    pid = info.get("pid", "?")
    return True, (
        f"incarnation-fence: {key} held by {holder} "
        f"(pid={pid}, {_describe_process(pid)}); "
        f"refusing outward action - another incarnation owns this lineage"
    )
