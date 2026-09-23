"""Task-grain claims: one claim per plan task under a node (epic, group 3).

Key shape ``task:<node-id>:<task-id>``. No primitive change: the claims core
validates a key only for non-emptiness and encoded filename length, and this
module is a thin namespace beside ``node:``/``dispatch:``/``walker:``/
``lane-slot:``. Repo-local like lane slots (the prefix is not a global-id
prefix), so every worktree of the project's repo coordinates on one store.

The claim is the status transition: ``task update`` acquires inside ``pending -> in_progress`` and releases on ``done``. PID-backed claims use pure PID liveness; pid-less threads get a two-hour lease. Holders are full session ids or ``FNO_WORKER_NAME`` (never UUIDv7 head-8).
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

TASK_CLAIM_TTL_MS = 7_200_000  # 2 hours, the node-claim default


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
    """Claim with PID liveness or a pid-less two-hour lease."""
    return acquire_claim(
        key=task_key(node_id, task_id),
        holder=holder,
        ttl_ms=None if pid is not None else TASK_CLAIM_TTL_MS,
        pid=pid,
        pid_unavailable=pid is None,
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


__all__ = ["TASK_PREFIX", "TASK_CLAIM_TTL_MS", "task_key", "acquire_task", "release_task"]
