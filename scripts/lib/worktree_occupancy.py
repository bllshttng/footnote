"""Classify the processes rooted in a worktree: holder or inert (x-0396).

The sweep's process step (``_wt_pids``) enumerates truthfully and decides
coarsely: any pid keeps the tree, forever, because nothing retires a leaked
test-fixture keeper, an orphaned Bash-tool shell, or a bg session whose job
already finished. This module names each hit and classifies it.

This is sweep-side tooling, so it lives beside worktree-status.py in
scripts/lib, not in cli/src/fno: that tree is shrink-only net
(check-file-budget.sh), and the growth here is the feature. The bridge
(worktree-occupancy.sh) runs this file as a script with cli/src on
PYTHONPATH, which is where the fno modules it composes live.

Fail closed: a hit the classifier cannot positively place is a holder.
Absence of a recognised holder is never proof a tree is free.
"""

from __future__ import annotations

import json
import os
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Dict, Iterable, List, Optional, Tuple

from fno.agents.keeper_lane import KEEPER_BIN_NAME, REAP
from fno.agents.session_truth import STALLED_AFTER_S

HOLDS = "holds"
INERT = "inert"
KEEP = "keep"
TERMINATE = "terminate"
RETIRE = "retire"

_TERMINAL_JOB_STATES = frozenset({"done", "stopped", "failed"})
_SHELL_NAMES = frozenset({"zsh", "bash", "sh"})
_SNAPSHOT_TOKEN = "~/.claude/shell-snapshots/"
_DESCENT_CAP = 32


@dataclass
class Hit:
    pid: int
    verdict: str  # HOLDS | INERT
    action: str  # KEEP | TERMINATE | RETIRE
    job_id: str = "-"
    reason: str = ""
    cmd: str = ""


def _argv(row: dict) -> List[str]:
    return [str(a) for a in (row.get("cmdline") or [])]


def _argv0(row: dict) -> str:
    argv = _argv(row)
    if argv:
        return argv[0].rsplit("/", 1)[-1]
    return str(row.get("name") or "unknown")


def _cmd(row: dict) -> str:
    argv = _argv(row)
    cmd = " ".join(argv) if argv else str(row.get("name") or "")
    # A TSV row is one physical line: a `zsh -c <script>` argv embeds real
    # newlines, and a row-counting bridge would read every one as a second row.
    return " ".join(cmd.split())


def _snapshot_orphan(row: dict, home: str) -> bool:
    """R3 shape: an orphaned claude Bash-tool shell sourcing a shell snapshot."""
    argv = _argv(row)
    if len(argv) < 3:
        return False
    if argv[0].rsplit("/", 1)[-1] not in _SHELL_NAMES:
        return False
    if argv[1] != "-c":
        return False
    home_prefix = f"source {str(home).rstrip('/')}/.claude/shell-snapshots/"
    return argv[2].startswith(home_prefix) or argv[2].startswith(f"source {_SNAPSHOT_TOKEN}")


def _direct_verdict(pid: int, row: dict, *, keeper_verdicts, job_of_pid, job_state, home) -> Optional[Hit]:
    """R1/R2/R3 over one row. None means no rule names this process."""
    argv = _argv(row)
    if argv and argv[0].rsplit("/", 1)[-1] == KEEPER_BIN_NAME:
        verdict = keeper_verdicts.get(pid) if keeper_verdicts else None
        if verdict is None:
            return Hit(pid, HOLDS, KEEP, reason="keeper lane unreadable, so the reap arm cannot read - no reap", cmd=_cmd(row))
        v, reason = verdict
        if v == REAP:
            return Hit(pid, INERT, TERMINATE, reason=reason, cmd=_cmd(row))
        return Hit(pid, HOLDS, KEEP, reason=reason, cmd=_cmd(row))

    cmd = _cmd(row)
    from fno.footprint import is_claude_spare_pool

    if is_claude_spare_pool(cmd):
        # The identity join is the daemon's rv socket farm, never a recorded
        # cwd. The agents-registry cwd field AND the job state.json cwd field
        # are both the SPAWN directory (measured 2026-09-11: 29 of 30 alive
        # registry rows and 17 of 17 live bg jobs read the canonical checkout
        # while working inside worktrees); a hold detector built on that field
        # named its own live worktree free. The row dicts from
        # orphans.iter_processes() carry a cwd key; it stays unread here.
        if job_of_pid is None:
            return Hit(pid, HOLDS, KEEP, reason="claude session: rv socket map unreadable", cmd=cmd)
        job_id = job_of_pid.get(pid)
        if job_id is None:
            return Hit(
                pid,
                HOLDS,
                KEEP,
                reason="claude session: no rv socket join (argv bg-spare is identical for a live session and an idle spare)",
                cmd=cmd,
            )
        state = job_state(job_id) if job_state else None
        if state is None:
            return Hit(pid, HOLDS, KEEP, job_id=job_id, reason=f"claude job {job_id}: state unreadable", cmd=cmd)
        job_state_name, age_s = state
        age_txt = "unreadable" if age_s is None else f"{int(age_s)}s"
        if job_state_name in _TERMINAL_JOB_STATES:
            if age_s is not None and age_s > STALLED_AFTER_S:
                return Hit(pid, INERT, RETIRE, job_id=job_id, reason=f"claude job {job_id} {job_state_name}, transcript silent {age_txt}", cmd=cmd)
            return Hit(pid, HOLDS, KEEP, job_id=job_id, reason=f"claude job {job_id} {job_state_name}, transcript {age_txt}", cmd=cmd)
        return Hit(pid, HOLDS, KEEP, job_id=job_id, reason=f"claude job {job_id} {job_state_name}, transcript {age_txt}", cmd=cmd)

    if int(row.get("ppid") or 0) == 1 and _snapshot_orphan(row, home):
        return Hit(pid, INERT, TERMINATE, reason="claude tool shell, owning session exited", cmd=cmd)

    return None


def classify(
    pids: Iterable[int],
    *,
    procs: Dict[int, dict],
    keeper_verdicts: Optional[Dict[int, Tuple[str, str]]],
    job_of_pid: Optional[Dict[int, str]],
    job_state: Optional[Callable[[str], Optional[Tuple[str, Optional[float]]]]],
    home: str,
    now: float,
) -> List[Hit]:
    """Classify each pid, first match wins: R0 no ps row, R1 keeper, R2 claude
    bg session, R3 orphaned tool shell, R4 descendant of any of these, R5
    unclassified holds."""
    del now  # reserved: rules read age through job_state, not the wall clock
    hits: List[Hit] = []
    for pid in pids:
        row = procs.get(pid)
        if row is None:
            hits.append(Hit(pid, HOLDS, KEEP, reason="no ps row"))
            continue
        direct = _direct_verdict(
            pid, row, keeper_verdicts=keeper_verdicts, job_of_pid=job_of_pid, job_state=job_state, home=home
        )
        if direct is None:
            direct = _descendant_verdict(
                row,
                procs,
                keeper_verdicts=keeper_verdicts,
                job_of_pid=job_of_pid,
                job_state=job_state,
                home=home,
            )
        if direct is None:
            hits.append(Hit(pid, HOLDS, KEEP, reason=f"unclassified: {_argv0(row)}", cmd=_cmd(row)))
            continue
        hits.append(direct)
    return hits


def _descendant_verdict(row, procs, *, keeper_verdicts, job_of_pid, job_state, home) -> Optional[Hit]:
    """R4: inherit the first classified ancestor's verdict and action."""
    pid = row.get("pid")
    cur = row.get("ppid")
    hops = 0
    while isinstance(cur, int) and cur > 0 and hops < _DESCENT_CAP and cur != pid:
        ancestor = procs.get(cur)
        if ancestor is None:
            return None
        hit = _direct_verdict(
            cur, ancestor, keeper_verdicts=keeper_verdicts, job_of_pid=job_of_pid, job_state=job_state, home=home
        )
        if hit is not None:
            return Hit(
                pid,
                hit.verdict,
                hit.action,
                job_id=hit.job_id,
                reason=f"child of {cur}: {hit.reason}",
                cmd=_cmd(row),
            )
        cur = ancestor.get("ppid")
        hops += 1
    return None


# --- live reader -------------------------------------------------------------


def _keeper_rows(procs: Dict[int, dict], pids: List[int]) -> List[dict]:
    """The keeper rows among the hits and their ancestors, for a bounded lane run."""
    want: List[dict] = []
    seen = set()
    stack = list(pids)
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        row = procs.get(pid)
        if row is None:
            continue
        argv = _argv(row)
        if argv and argv[0].rsplit("/", 1)[-1] == KEEPER_BIN_NAME:
            want.append(row)
        ppid = row.get("ppid")
        if isinstance(ppid, int):
            stack.append(ppid)
    return want


def _job_state_reader(home: str, now: float) -> Callable[[str], Optional[Tuple[str, Optional[float]]]]:
    def read(job_id: str) -> Optional[Tuple[str, Optional[float]]]:
        path = Path(home) / ".claude" / "jobs" / job_id / "state.json"
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except Exception:  # noqa: BLE001 - unreadable state keeps the tree
            return None
        state = data.get("state")
        if not isinstance(state, str):
            return None
        scan = data.get("linkScanPath")
        age_s: Optional[float] = None
        if isinstance(scan, str) and scan:
            try:
                age_s = max(0.0, now - os.stat(scan).st_mtime)
            except OSError:
                age_s = None
        return state, age_s

    return read


def main(argv: List[str]) -> int:
    """``python scripts/lib/worktree_occupancy.py <worktree> <pid>...``

    One tab-separated row per pid: ``pid verdict action job reason cmd``.
    Any failure exits 2 with nothing on stdout, so the bridge fails closed."""
    try:
        from fno.agents.orphans import iter_processes
    except Exception:  # noqa: BLE001
        return 2
    try:
        worktree = argv[0]
        pids = [int(a) for a in argv[1:]]
        home = os.path.expanduser("~")
        now = time.time()

        procs: Dict[int, dict] = {}
        for info in iter_processes():
            pid = info.get("pid")
            if isinstance(pid, int):
                procs[pid] = info

        from fno.agents import keeper_lane
        from fno.agents.session_procs import bg_socket_pid_map

        keeper_verdicts = None
        if pids:
            rows = _keeper_rows(procs, pids)
            keeper_verdicts = keeper_lane.discover(iter_fn=lambda: iter(rows)).verdicts

        socket_map = bg_socket_pid_map()
        job_of_pid: Optional[Dict[int, str]] = None
        if socket_map:
            job_of_pid = {pid: job for job, pid in socket_map.items()}

        hits = classify(
            pids,
            procs=procs,
            keeper_verdicts=keeper_verdicts,
            job_of_pid=job_of_pid,
            job_state=_job_state_reader(home, now),
            home=home,
            now=now,
        )
        del worktree  # the enumeration is cwd-based; the tree names the hits
    except Exception:  # noqa: BLE001 - a broken classifier must read as broken
        return 2
    out = []
    for h in hits:
        out.append(f"{h.pid}\t{h.verdict}\t{h.action}\t{h.job_id}\t{h.reason}\t{h.cmd}")
    if out:
        sys.stdout.write("\n".join(out) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
