"""Age-based rename-steal for the cross-language mkdir mutexes.

A ``mkdir``-style mutex is atomic to acquire but carries no liveness signal: a
directory left behind by a killed holder is byte-identical to one a live worker
is inside. On 2026-07-13 that starved every ``events.jsonl`` append for 8 days
and made one claim key permanently unrecoverable.

The lock dir's own mtime IS its acquisition timestamp, so age is the liveness
predicate. Removal only ever happens via an atomic rename the remover won, so
two stealers can never both conclude they cleared the same corpse.

Mirrored in ``crates/fno-agents/src/claims.rs`` (``STALE_MUTEX_STEAL`` /
``steal_if_stale``); the ``.recovery.d`` mutex is wire protocol between the two,
so the threshold and the steal rule must change in lockstep.
"""

from __future__ import annotations

import logging
import os
import shutil
import time
from pathlib import Path

log = logging.getLogger(__name__)

# Every critical section under these mutexes is sub-second (append one line;
# rename one file and exclusive-create another), so 120s is ~100x the honest
# hold time. Never do slow work (network, subprocess) inside one of these locks
# or this threshold stops being a corpse detector.
STALE_MUTEX_STEAL_S = 120


def steal_if_stale(lock_dir: Path, *, threshold_s: float = STALE_MUTEX_STEAL_S) -> bool:
    """Rename-steal ``lock_dir`` when it is older than ``threshold_s``.

    Returns True when the caller should retry its ``mkdir`` immediately: either
    the corpse was stolen, or the lock turned out to be gone already. False
    means the lock is honestly held and the caller should wait exactly as it
    did before.
    """
    try:
        age = time.time() - lock_dir.stat().st_mtime
    except OSError:
        return True  # vanished between contention and stat: freed, retry

    if age <= threshold_s:
        return False

    reaped = lock_dir.with_name(f"{lock_dir.name}.reap.{os.getpid()}")
    shutil.rmtree(reaped, ignore_errors=True)  # our own leftover from a prior steal
    try:
        os.rename(lock_dir, reaped)
    except FileNotFoundError:
        return True  # released while we were looking
    except OSError:
        return False  # another stealer won; fall back to the normal wait loop

    log.warning("stole stale mutex %s (age %ds) -> %s", lock_dir, int(age), reaped)
    shutil.rmtree(reaped, ignore_errors=True)  # inert if it fails
    return True
