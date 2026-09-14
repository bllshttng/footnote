"""Start workers in panes placed by a separate mux verb."""
from __future__ import annotations

import shlex
import subprocess
from typing import Callable

from fno.agents.dispatch import DispatchAskError

Runner = Callable[..., subprocess.CompletedProcess[str]]


def pane_placement_conflict(pane: int | None, **placements) -> str | None:
    if pane is None:
        return None
    labels = {"workspace": "--workspace", "split": "--split", "at": "--at", "tab": "--tab", "bounded": "--bounded-placement", "tab_id": "--tab-id"}
    flag = next((labels[name] for name, value in placements.items() if value is not None and value is not False), None)
    return f"--pane cannot be combined with {flag}; it targets an already-placed pane" if flag else None


def resolve_existing_pane(session: str, pane_id: int, rows: list[dict]) -> dict:
    if pane_id < 1:
        raise DispatchAskError(f"--pane needs a positive pane id, got {pane_id}", exit_code=2)
    row = next((item for item in rows if item.get("pane_id") == pane_id), None)
    if row is None:
        raise DispatchAskError(f"--pane {pane_id} was not found in mux session {session!r}", exit_code=2)
    if row.get("fno_id"):
        raise DispatchAskError(f"--pane {pane_id} is occupied by worker {row['fno_id']!r}", exit_code=2)
    if row.get("pristine_idle_shell") is not True:
        raise DispatchAskError(f"--pane {pane_id} is not a confirmed pristine idle shell", exit_code=2)
    return row


def start_existing_pane(session: str, pane_id: int, cwd: str, wrapped: list[str], run_mux: Runner, runner: Runner) -> subprocess.CompletedProcess[str]:
    proc = run_mux(
        [
            "mux", "pane", "send", "--server", session, str(pane_id), "--text", "cd -- " + shlex.quote(cwd) + " && exec " + shlex.join(wrapped), "--submit", "--raw", "--guarded",
        ],
        runner,
    )
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()
        raise DispatchAskError(f"existing pane {pane_id} rejected the worker start in session {session!r}: {detail or 'no output'}", exit_code=1)
    return proc
