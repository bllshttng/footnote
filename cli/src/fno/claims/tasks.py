"""Task-grain claims: one claim per plan task under a node (epic x-09d7, group 3).

Key shape ``task:<node-id>:<task-id>``. No primitive change: the claims core
validates a key only for non-emptiness and encoded filename length, and this
module is a thin namespace beside ``node:``/``dispatch:``/``walker:``/
``lane-slot:``. Repo-local like lane slots (the prefix is not a global-id
prefix), so every worktree of the project's repo coordinates on one store.

The claim IS the status transition, never a standalone verb a worker calls
first: the backlog ``task update`` sub-typer takes it inside
``pending -> in_progress`` and releases it on ``done``. Liveness is pure
pid-anchored (``ttl_ms=None``, the ``reconcile_lane_slot`` shape): a dead
worker's task frees the instant its process is gone, and ``acquire_claim``'s
stale-recovery step archives the corpse and retries. The holder is the FULL
harness session id - a codex UUIDv7 head-8 is a ~65.5s clock bucket, so two
codex workers spawned in one minute would share a handle and the second would
re-acquire the first's task as idempotent.
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

from .core import acquire_claim, release_claim
from .types import Claim

#: Task claim key namespace. Not a global-id prefix (see
#: claims.io._GLOBAL_ID_PREFIXES), so task claims stay repo-local and
#: coordinate across the project's worktrees like lane slots.
TASK_PREFIX = "task:"


def task_key(node_id: str, task_id: str) -> str:
    """The claim key for one task of one node."""
    return f"{TASK_PREFIX}{node_id}:{task_id}"


def acquire_task(
    node_id: str,
    task_id: str,
    holder: str,
    *,
    pid: Optional[int],
    harness: Optional[str] = None,
    root: Optional[Path] = None,
) -> Claim:
    """Claim a task for ``holder``, pid-liveness with no TTL.

    ``pid`` is the durable harness session pid (``resolve_session_pid``). The
    backlog verb REFUSES an unprovable pid (exit 4) rather than pass ``None``
    through: core would anchor the claim to the short-lived CLI process, which
    dies on exit and leaves the claim instantly stealable. Raises
    :class:`fno.claims.core.ClaimHeldByOther` naming the live holder when a
    peer owns the task, :class:`fno.claims.core.ClaimContended` when the
    recovery mutex stays busy past its retry budget.
    """
    return acquire_claim(
        key=task_key(node_id, task_id),
        holder=holder,
        ttl_ms=None,  # pure pid-liveness: frees the instant the worker dies
        pid=pid,
        harness=harness,
        reason=f"task {task_id} of node {node_id}",
        metadata={"node": node_id, "task": task_id},
        root=root,
    )


def release_task(
    node_id: str,
    task_id: str,
    holder: str,
    *,
    root: Optional[Path] = None,
) -> None:
    """Release the task claim we hold. Idempotent, non-strict (see core)."""
    release_claim(key=task_key(node_id, task_id), holder=holder, root=root)


__all__ = ["TASK_PREFIX", "task_key", "acquire_task", "release_task"]
