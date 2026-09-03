"""Portal placement for the thread substrate (x-9b60): one spawn call ends
with the portal open where the caller asked.

Its own module by budget necessity (mux_spawn is over the file budget and
may only shrink) and by name: this answers exactly one question - how a
spawned thread worker gets shown through a portal. It builds the same argv
a manual second command would type, the ``fno mux thread`` reach, so no
parallel portal path exists to drift."""

from __future__ import annotations

import subprocess
from typing import Callable, Optional

from fno.agents.mux_spawn import DispatchAskError, _run_mux


def place_thread_portal(
    name: str,
    portal: int,
    *,
    workspace: Optional[str] = None,
    split: Optional[str] = None,
    at: Optional[str] = None,
    tab: Optional[str] = None,
    runner: Callable[..., "subprocess.CompletedProcess[str]"] = subprocess.run,
) -> str:
    """Open portal ``portal`` showing ``name`` through the one door that
    creates a portal - the ``fno mux thread`` reach - carrying the placement
    the caller named (x-9b60). One call, one door: this sends the same
    ThreadPane verb a second manual command would, so no parallel portal
    path exists to drift.

    Returns the landing notice. Raises DispatchAskError on any refusal; the
    worker itself is already live, so the caller reports this failure AFTER
    its spawn receipt, never instead of it.
    """
    args = ["mux", "thread", name, "--portal", str(int(portal))]
    if workspace:
        args += ["--workspace", workspace]
    if split:
        args += ["--split", split]
    if at:
        args += ["--at", at]
    if tab:
        args += ["--tab", tab]
    proc = _run_mux(args, runner)
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "no output").strip()
        raise DispatchAskError(
            f"portal {portal} placement failed: {detail} (the worker is "
            f"live; place it with 'fno mux thread {name} --portal {portal}')",
            exit_code=1,
        )
    return (proc.stdout or "").strip()
