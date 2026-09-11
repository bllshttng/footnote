"""Reap, and account for, processes rooted in a pytest session's tmp tree.

A test that starts a real provider binary can leave a live daemon behind it:
the daemon calls setsid, detaches to ppid 1, and outlives the run (x-ec81
measured three ``claude daemon run`` processes, each rooted in a deleted
pytest garbage directory, pinned by its own bg-spare tree). Enumeration goes
through ``fno.agents.orphans.iter_processes``, the one shared
``psutil.process_iter`` site, so this census and the orphan sweep can never
drift apart.

# ponytail: only orphans (ppid in reaper_set) get a cwd read, so a leak
# still parented by a live launcher is invisible here until that launcher
# exits. Upgrade path: a full cwd read over every row, at ~5x the census cost.
"""
from __future__ import annotations

import os
import signal
from typing import Callable

_REAP_WAIT_TERM_S = 3
_REAP_WAIT_KILL_S = 3


def _matches(cwd: str | None, roots: list[str], match_component: str | None) -> bool:
    if not cwd:
        return False
    for root in roots:
        prefix = root.rstrip("/") + "/"
        if not cwd.startswith(prefix):
            continue
        if match_component is None:
            return True
        rest = cwd[len(prefix):]
        head = rest.split("/", 1)[0]
        if head.startswith(match_component):
            return True
    return False


def reap_rooted(
    roots,
    *,
    match_component: str | None = None,
    reaper: int = 1,
) -> list[dict]:
    """SIGTERM, then SIGKILL, every process whose cwd lies under ``roots``,
    together with its full descendant tree; return one row per signalled pid.

    ``match_component`` narrows the match to roots whose FIRST path component
    under ``root`` starts with the prefix (``garbage-``): a live ``pytest-N``
    dir from another session shares the ``pytest-of-<user>`` ancestor and must
    not match. Descendants are captured BEFORE the first signal - a killed
    parent reparents its children, and a capture after TERM reads a different
    tree. ``terminal`` reports the measured end state per pid, not an
    assumption: a process that ignored both signals reads ``False``.
    """
    import psutil

    from fno.agents.orphans import iter_processes

    normalized = [os.path.abspath(str(r)) for r in roots]
    matched = [
        row for row in iter_processes(reaper)
        if _matches(row.get("cwd"), normalized, match_component)
    ]

    tree_pids: dict[int, int] = {}  # pid -> root pid of its tree
    for row in matched:
        root_pid = row["pid"]
        tree_pids[root_pid] = root_pid
        try:
            for child in psutil.Process(root_pid).children(recursive=True):
                tree_pids.setdefault(child.pid, root_pid)
        except psutil.NoSuchProcess:
            pass

    report: list[dict] = []
    procs = []
    for pid, root_pid in tree_pids.items():
        row = next((r for r in matched if r["pid"] == pid), None)
        try:
            proc = psutil.Process(pid)
        except psutil.NoSuchProcess:
            continue
        procs.append(proc)
        report.append({
            "pid": pid,
            "ppid": row["ppid"] if row else None,
            "cwd": row.get("cwd") if row else None,
            "cmdline": row.get("cmdline") if row else None,
            "root_pid": root_pid,
            "terminal": False,
        })
    if not procs:
        return report

    for proc in procs:
        try:
            proc.send_signal(signal.SIGTERM)
        except psutil.NoSuchProcess:
            pass
    _, survivors = psutil.wait_procs(procs, timeout=_REAP_WAIT_TERM_S)
    for proc in survivors:
        try:
            proc.kill()
        except psutil.NoSuchProcess:
            pass
    psutil.wait_procs(survivors, timeout=_REAP_WAIT_KILL_S)

    # A zombie is dead-but-uncollected: its parent (this test process) has not
    # wait()ed yet. Reading is_running() alone would report a killed child as
    # alive, so the zombie status counts as terminal here.
    def _terminal(proc) -> bool:
        try:
            return proc.status() == psutil.STATUS_ZOMBIE or not proc.is_running()
        except psutil.NoSuchProcess:
            return True

    terminal_now = {p.pid: _terminal(p) for p in procs}
    for row in report:
        row["terminal"] = bool(terminal_now.get(row["pid"], False))
    return report
