"""How ``rm_agent`` measures a removed worktree's size without an unbounded
wait (x-4775). Extracted from dispatch.py: it is over the file-budget gate's
5,000-line ceiling and may only shrink.
"""
from __future__ import annotations

import os
import time
from typing import Optional

# Wall-clock budget for `directory_bytes`: the last unbounded wait in the rm
# path. Mirrors `DIRECTORY_BYTES_BUDGET` in daemon.rs; this route is a
# one-shot process with no executor to starve, so only the bound and the
# `None`-not-zero fix are ported here, not the off-executor scheduling change.
DIRECTORY_BYTES_BUDGET_S = 10.0


def directory_bytes(path: str, *, budget_s: float = DIRECTORY_BYTES_BUDGET_S) -> Optional[int]:
    """Recursively sum file sizes under ``path``, bounded by ``budget_s``.

    Checked once per directory entered, not per file, so the deadline check's
    own cost never dominates the walk. ``budget_s`` is a parameter (rather
    than the constant baked in) so a test can prove the deadline fires
    without waiting the real duration.
    """
    deadline = time.monotonic() + budget_s
    total = 0
    try:
        for root, dirs, files in os.walk(path, followlinks=False):
            if time.monotonic() >= deadline:
                return None
            dirs[:] = [name for name in dirs if not os.path.islink(os.path.join(root, name))]
            for name in files:
                total += os.lstat(os.path.join(root, name)).st_size
    except OSError:
        return None
    return total
