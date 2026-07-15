"""Advisory cross-process lock serializing fno's two plan-doc writers.

``fno plan stamp`` (the ship gate) and status_fanout's ``_append_plan_progress``
both do a whole-file read-modify-write on a node's plan doc. Atomic writes stop
torn reads but NOT lost updates, so a stamp landing during a progress append
silently drops one side unless the two serialize.

The lock is a sidecar flock, NEVER the plan file's own fd: both writers finish
with ``os.replace``, which swaps the plan's inode out from under any lock held on
that inode, so the next locker would lock a now-detached inode and fail to
serialize. Keying a separate lockfile on the plan's resolved path avoids that.

Advisory only - human editors are unguarded by design (two fno writers). Stdlib
only (fcntl/hashlib/os): ``fno.plan._stamp`` runs under a typer-less interpreter
from Rust finalize, so this must not pull any third-party import.
"""
from __future__ import annotations

import fcntl
import hashlib
import os
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator

from fno import paths


@contextmanager
def plan_doc_lock(path: Path, timeout: float = 2.0) -> Iterator[None]:
    """Hold an exclusive advisory lock keyed on the plan doc's resolved path.

    Polls ``flock(LOCK_EX | LOCK_NB)`` until ``timeout``, then raises TimeoutError
    (the append caller swallows it and skips the tick; the stamp caller surfaces
    it). The lock is held only for one whole-file rewrite, so contention past the
    timeout is not expected in practice.
    """
    lock_dir = paths.locks_dir()  # ~/.fno/locks; config-free so bare-python _stamp works
    lock_dir.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha1(str(Path(path).resolve()).encode()).hexdigest()
    lock_path = lock_dir / f"plan-{digest}.lock"
    deadline = time.monotonic() + timeout
    fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o644)
    try:
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except OSError:
                if time.monotonic() >= deadline:
                    raise TimeoutError(f"plan_doc_lock: {lock_path} busy > {timeout}s")
                time.sleep(0.02)
        try:
            yield
        finally:
            fcntl.flock(fd, fcntl.LOCK_UN)
    finally:
        os.close(fd)


if __name__ == "__main__":  # ponytail: runnable self-check, no framework
    import tempfile

    with tempfile.TemporaryDirectory() as d:
        p = Path(d) / "plan.md"
        p.write_text("x")
        with plan_doc_lock(p):
            # Re-entrant acquire from the SAME process must time out fast: flock is
            # per-fd, and a second fd on the same lockfile cannot get LOCK_EX.
            try:
                with plan_doc_lock(p, timeout=0.1):
                    raise AssertionError("nested acquire should have timed out")
            except TimeoutError:
                pass
    print("ok")
