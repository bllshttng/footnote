"""Assign a bound codex thread to its repo's codex project.

codex derives no project from cwd, so an unassigned thread reads in the
ChatGPT desktop and mobile apps as its own Project keyed on the worktree
path - for fno worktrees, a Project named after the node id. A thread's
project is an explicit ``projectId``, and the codex CLI exposes no argv
flag to set it, so the assignment rides the ``fno-agents`` binary's
hidden ``codex-assign-project`` verb: it resolves or creates the repo's
project and patches the bound thread with ``thread/metadata/update``.
"""

from __future__ import annotations

import subprocess
from pathlib import Path


def assign_project_detached(cwd: Path, session_id: str) -> None:
    """Assign a bound headless codex thread to its repo's codex project.

    Fire-and-forget by contract: this execs the verb detached and never
    waits, and the verb itself is fail-open (an absent daemon, a refused
    write, or a cwd outside a repo leaves the thread unassigned, which is
    the pre-assignment behavior). A missing binary or a refused spawn is
    equally silent - a project we cannot assign must never delay or fail
    a spawn that has already landed.
    """
    if not session_id:
        return
    try:
        from fno.rust_binary import resolve_binary

        binary = resolve_binary()
        if binary is None:
            return
        subprocess.Popen(
            [str(binary), "codex-assign-project", "--cwd", str(cwd), "--thread-id", session_id],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
        )
    except Exception:
        # Fail-open is the contract (see docstring); this never blocks a spawn.
        pass
