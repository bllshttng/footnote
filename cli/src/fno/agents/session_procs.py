"""Resolve a registry/roster row to the process that IS the session (x-3f84 W2).

For a claude bg row the recorded pid names the PTY HOST (`claude bg-pty-host`),
not the worker: measured 2026-08-22, row 55f9847a carried pid 98779 whose `ps`
comm was `bg-pty-host` hours older than the worker. The session's own process
is found through the claude daemon's rendezvous socket farm:
``/tmp/cc-daemon-<uid>/<daemon-id>/rv/<short>.sock``, one socket per live bg
session, whose lsof holder is the process that BECOMES the session. The socket
stem is the claude jobId, which by construction is the first 8 hex of the
session uuid - the same string the registry carries as a claude row's
``short_id``, so the join key needs no derivation.

One lsof call prices the whole farm; every consumer (census, ``agents top``,
roster discovery) shares the same map so the cost view and the gate cannot
disagree about which process a row owns.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Optional

_MIB = 1024 * 1024


def rv_socket_root() -> Path:
    """The rendezvous socket farm root (``FNO_CC_DAEMON_RV_ROOT`` overrides it)."""
    override = os.environ.get("FNO_CC_DAEMON_RV_ROOT")
    return Path(override) if override else Path(f"/tmp/cc-daemon-{os.getuid()}")


def _pid_map_from_lsof(text: str) -> dict[str, int]:
    """Parse ``lsof -F pfn`` output into ``{socket stem: pid}``.

    The stem of the ``n`` line's path is the session's 8-hex jobId. A process
    block repeats one ``p`` line and then an ``f``/``n`` pair per descriptor
    (one holder measured with two fds on the same socket), so the first pid
    seen for a stem wins and later pairs for the same stem are dropped.
    """
    out: dict[str, int] = {}
    pid: Optional[int] = None
    for line in text.splitlines():
        if line.startswith("p") and line[1:].isdigit():
            pid = int(line[1:])
        elif line.startswith("n") and pid is not None:
            stem = Path(line[1:]).stem
            if stem and stem not in out:
                out[stem] = pid
    return out


def bg_socket_pid_map(root: Optional[Path] = None) -> dict[str, int]:
    """8-hex claude jobId -> pid of the process that IS that bg session.

    Best-effort by design: a missing farm, a dead lsof, or a timeout all mean
    ``{}`` and the caller keeps the pid it recorded. That undercounts a bg
    row's cost - the defect this module exists to close - so callers that
    gate on cost must treat an empty map as "unknown", never as "no sessions".
    """
    base = root if root is not None else rv_socket_root()
    try:
        socks = sorted(base.glob("*/rv/*.sock"))
    except OSError:
        return {}
    if not socks:
        return {}
    try:
        proc = subprocess.run(
            ["lsof", "-F", "pfn", "--", *[str(s) for s in socks]],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (OSError, subprocess.SubprocessError):
        return {}
    # lsof exits 1 when no listed file has a holder; its stdout is then empty
    # and parses to {}, which is the correct answer for "no live bg sessions".
    return _pid_map_from_lsof(proc.stdout)


def resolve_session_pid(
    *,
    harness: str,
    short_id: str = "",
    session_id: Optional[str] = None,
    pid: Optional[int] = None,
    socket_map: Optional[dict[str, int]] = None,
) -> Optional[int]:
    """The pid of the process that IS the session, falling back to the recorded pid.

    Only claude bg sessions have the host/session split (codex and pane workers
    are their own process); every other harness returns ``pid`` unchanged.
    """
    if harness == "claude":
        key = short_id or (session_id or "")[:8]
        if key:
            m = bg_socket_pid_map() if socket_map is None else socket_map
            hit = m.get(key)
            if hit:
                return hit
    return pid


def tree_rss_mb(pid: Optional[int], _psutil=None) -> Optional[int]:
    """RSS of the whole process tree rooted at ``pid``, in MiB.

    A worker forks helpers the row's own pid never accounts for: per-session
    stdio MCP servers and fno helpers. ``top``'s cost column and any
    process-cost gate read this same function, so the two cannot disagree
    about what a row costs. A dead pid or a psutil-less host returns None.
    """
    if not pid:
        return None
    try:
        psutil = _psutil or __import__("psutil")
        proc = psutil.Process(pid)
        total = proc.memory_info().rss
        for child in proc.children(recursive=True):
            try:
                total += child.memory_info().rss
            except psutil.Error:
                continue  # a child that died mid-scan is cost that already left
        return int(total // _MIB)
    except Exception:
        return None
