"""Resolve a registry/roster row to the process that IS the session (x-3f84 W2).

For a claude bg row the recorded pid names the PTY HOST (`claude bg-pty-host`),
not the worker: measured 2026-08-22, row 55f9847a carried pid 98779 whose `ps`
comm was `bg-pty-host` hours older than the worker. The session's own process
is found through the claude daemon's rendezvous socket farm:
``/tmp/cc-daemon-<uid>/<daemon-id>/rv/<short>.sock``, one socket per live bg
session, whose lsof holder is the process that BECOMES the session. The socket
stem is the claude jobId, the first 8 hex of the session uuid.

One lsof call prices the whole farm; every consumer (census, ``agents top``,
roster discovery) shares the same map so the cost view and the gate cannot
disagree about which process a row owns.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Optional

from fno.harness_identity import claude_transport_short_id

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


def bg_socket_pid_map(
    root: Optional[Path] = None, *, timeout: float = 15.0
) -> dict[str, int]:
    """8-hex claude jobId -> pid of the process that IS that bg session.

    Best-effort by design: a missing farm, a dead lsof, or a timeout all mean
    ``{}``, which undercounts a bg row's cost, so callers that gate on cost
    must treat an empty map as "unknown", never as "no sessions".
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
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError):
        return {}
    # lsof exits 1 when no listed file has a holder; its stdout is then empty
    # and parses to {}, which is the correct answer for "no live bg sessions".
    return _pid_map_from_lsof(proc.stdout)


def roster_pid_map() -> Optional[dict[str, Optional[int]]]:
    """The claude daemon roster as ``{8-hex jobId: host pid}``, or None when unreadable.

    The SECOND daemon-side oracle for a bg row the rv farm missed: a short_id
    in neither map is a dead session (x-a457); a roster pid is the PTY HOST
    hosting it. Missing file: {}; other failure: None. A held worker with no
    usable pid maps to None - the session exists, its key is not absence.
    """
    from fno.agents.spawn_gate import _roster_path

    try:
        raw = json.loads(_roster_path().read_text(encoding="utf-8"))
    except FileNotFoundError:
        return {}
    except Exception:  # noqa: BLE001 - an unreadable roster proves nothing
        return None
    workers = raw.get("workers") if isinstance(raw, dict) else None
    if not isinstance(workers, dict):
        return None
    out: dict[str, Optional[int]] = {}
    for w in workers.values():
        if not isinstance(w, dict) or not isinstance(w.get("sessionId"), str):
            continue
        pid = w.get("pid")
        key = claude_transport_short_id(w["sessionId"])
        out[key] = pid if isinstance(pid, int) and not isinstance(pid, bool) else None
    return out


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
