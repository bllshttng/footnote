"""Resolve a durable session pid for the hybrid liveness pid-arm (ab-cc5553f2).

The ``node:<id>`` claim is acquired with ``--ttl`` AND ``--pid <durable>``. The
durable pid must be the process that lives as long as the *session*, not the
transient ``fno`` python subprocess that runs ``fno target init`` (that pid is
dead ~1s after init returns - the original bug). Every dispatch mode (attended
``/target``, ``claude --bg`` autolaunch, megawalk worker, handoff successor)
runs ``/target`` under a ``claude`` process, so the uniform mechanism is a
process-tree walk from init up the parent chain to the nearest ``claude``
ancestor.

This is degrade-safe by construction: if no ``claude`` ancestor is found, the
caller records no ``--pid`` (or the transient default) and the claim is LIVE via
the TTL arm exactly as before. A mis-resolved/transient pid is a dead pid that
fails ``is_live`` -> STALE on expiry, indistinguishable from a missing one. The
pid arm only ever *extends* liveness, so it is structurally impossible to
regress.
"""
from __future__ import annotations

import os
from typing import Optional

import psutil

# How far up the parent chain to look before giving up. The real chain is
# short (claude -> ... -> fno -> bash init), but bg/handoff nesting can add a
# few levels; 25 is generous and bounds a pathological/looping ancestry.
_MAX_DEPTH = 25


def _matches_claude(proc: psutil.Process) -> bool:
    """True iff PROC is a ``claude`` process.

    The versioned binary's ``name()`` is the version string (e.g. ``2.1.177``),
    so a basename match alone misses it; the executable PATH still contains
    ``claude`` (``.../share/claude/versions/2.1.177`` or ``.../bin/claude``).
    Match a case-insensitive ``claude`` substring in either name or exe path.
    """
    for getter in (proc.name, proc.exe):
        try:
            value = getter()
        except (psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess):
            continue
        if value and "claude" in value.lower():
            return True
    return False


def resolve_session_pid(from_pid: Optional[int] = None) -> Optional[int]:
    """Return the durable session pid, or None if uncapturable (degrade).

    Resolution order:
      1. ``FNO_SESSION_PID`` env, if set to a live pid (launcher override).
      2. The nearest ``claude`` ancestor of FROM_PID (default: this process's
         parent - the caller passes its own pid chain up to the session).

    Returns None when neither yields a live pid, so the caller degrades to
    TTL-only liveness (today's behavior).
    """
    env = os.environ.get("FNO_SESSION_PID", "").strip()
    if env:
        try:
            env_pid = int(env)
            # env_pid > 0: pid_exists(0)/(-1) can be True on some POSIX systems
            # (kill(0/-1, 0) semantics), and neither is a valid session pid.
            if env_pid > 0 and psutil.pid_exists(env_pid):
                return env_pid
        except (ValueError, OverflowError):
            pass  # malformed override -> fall through to the walk

    # Only ANCESTORS of the init subprocess are walked, and the genuine session
    # `claude` is always the first claude hit in that chain, so a binary that
    # merely lives under a `.claude/` path elsewhere cannot yield a wrong pid.
    start = from_pid if from_pid is not None else os.getppid()
    try:
        proc: Optional[psutil.Process] = psutil.Process(start)
    except (psutil.NoSuchProcess, psutil.AccessDenied, ValueError):
        # ValueError covers a negative/zero start pid; all degrade to None.
        return None

    depth = 0
    while proc is not None and depth < _MAX_DEPTH:
        if _matches_claude(proc):
            return proc.pid
        try:
            proc = proc.parent()
        except (psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess):
            return None
        depth += 1
    return None


__all__ = ["resolve_session_pid"]
