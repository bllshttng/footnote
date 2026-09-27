"""Resolve the durable session pid (and harness) for the liveness pid-arm.

The ancestor walk itself lives in Rust
(`spawn_context::session_identity_from_table` / `session_identity_ambient`),
served natively by `fno agents claim session-pid`. This module is the Python
shim over that verb: one cached exec per ``from_pid`` per process, so many
call sites pay one exec, and the pid and the harness can never name different
processes (AC6: both halves come from one JSON read).

The ``node:<id>`` claim is acquired with ``--ttl`` AND ``--pid <durable>``. The
durable pid must be the process that lives as long as the *session*, not the
transient ``fno`` python subprocess that runs ``fno do target init`` (that pid is
dead ~1s after init returns - the original bug). Every agent harness runs its
session under a long-lived binary (``claude``, ``codex``, ``gemini``,
``opencode``, ``agy``), so the uniform mechanism is a process-tree walk from
init up the parent chain to the nearest *harness* ancestor. The walk refuses
Claude Code pool machinery (a ``claude bg-spare``, a ``bg-pty-host``, the
daemon): the spare outlives every session it serves, so a pid answered there
pins a claim to machinery that never dies. A thread worker therefore degrades
to no pid; the claim lives by its TTL (the same answer the walk gave a plain
shell before).

This is degrade-safe by construction: if the verb answers nothing (no harness
ancestor, a refused spare, the binary unavailable), the caller records no
``--pid`` and the claim is LIVE via the TTL arm exactly as before. A
mis-resolved/transient pid is a dead pid that fails ``is_live`` -> STALE on
expiry, indistinguishable from a missing one.

The ancestor is an ancestor of the acquiring process, so it dies no later than
that process. It dies no later than the SESSION only when the harness forks one
binary per session. codex does not: its ancestor is a shared ``codex
app-server`` that hosts every session on the machine and outlives all of them,
so a pid answering there proves the multiplexer lives and says nothing about the
session. :func:`pid_dies_with_session` is the gate that separates the two, and
the provenance stamp is its one consumer - see ``_resolve_pid_provenance`` in
``core.py``. Nothing else here may read a live ancestor as a live session.
"""
from __future__ import annotations

import functools
import json
import subprocess
from typing import Optional

# Harnesses whose sessions SHARE one host process. For these the nearest harness
# ancestor is a multiplexer that outlives every session it hosts, so its
# liveness says nothing about the session's. Membership is proved by
# MEASUREMENT, never by reading a name: run one session, walk to its harness
# ancestor, end the session, and check whether that pid is still alive.
#   codex:    MEASURED 2026-09-03. `codex app-server --remote-control` pid
#             53566, up 9h20m, still answering for a session dead 5h, keeping a
#             lease live 3h45m past its TTL. ON the list.
#   opencode: MEASURED 2026-09-04, opencode 1.14.50. `opencode run` forked pid
#             26245 whose exe IS the session, and the process table held zero
#             opencode processes four seconds after it exited (the same probe
#             printed two rows while it ran, which is the positive control that
#             it can see them at all). Per-invocation, so OFF the list. Its
#             `serve`/`attach` lane could share a host, but fno dispatches
#             opencode one-shot and never through attach; measure again if that
#             changes.
# The other harnesses are unmeasured and therefore off the list by default, per
# the deny-list rule below.
_SHARED_HOST_HARNESSES = frozenset({"codex"})


def pid_dies_with_session(harness: Optional[str]) -> bool:
    """True when HARNESS forks a process per session, so its pid's death is the
    session's death.

    False ONLY for a harness measured to share one host process. An unknown or
    absent harness returns True and keeps today's behavior: the hybrid arm
    exists to stop a peer stealing the claim of a suspended-but-alive session,
    and this predicate must never widen that theft to harnesses nobody
    measured. So the list is a deny-list, and adding to it costs a measurement.
    """
    return (harness or "").strip().lower() not in _SHARED_HOST_HARNESSES


@functools.lru_cache(maxsize=None)
def _session_identity(from_pid: Optional[int]) -> tuple[Optional[int], Optional[str]]:
    """One `fno agents claim session-pid --json` read, cached per ``from_pid``
    for the process's lifetime. Env changes after the first call are not seen;
    the stamp pair (`FNO_SESSION_PID` / `FNO_SESSION_HARNESS`) is applied
    Rust-side on the exec, with the rules the verb's docstring states.

    Any failure to read - the verb missing, a non-zero exit, a malformed
    payload - degrades to ``(None, None)``, the uncapturable answer, never an
    exception into a caller that holds a claim lock.
    """
    cmd = ["fno", "agents", "claim", "session-pid", "--json"]
    if from_pid is not None:
        cmd += ["--from-pid", str(from_pid)]
    try:
        proc = subprocess.run(  # noqa: S603 - a fixed verb, never user input
            cmd,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        payload = json.loads(proc.stdout)
    except (OSError, ValueError, subprocess.SubprocessError):
        return (None, None)
    if not isinstance(payload, dict):
        return (None, None)
    pid = payload.get("session_pid")
    harness = payload.get("harness")
    return (
        pid if isinstance(pid, int) and pid > 0 else None,
        harness if isinstance(harness, str) and harness else None,
    )


def resolve_session_pid(from_pid: Optional[int] = None) -> Optional[int]:
    """Return the durable session pid, or None if uncapturable (degrade).

    Resolution order (applied by the native verb this shim execs):
      1. ``FNO_SESSION_PID`` env, if set to a live pid (launcher override).
      2. The nearest harness ancestor of FROM_PID (default: this process's
         parent - the caller passes its own pid chain up to the session) that
         is not Claude Code pool machinery.

    Returns None when neither yields a live pid, so the caller degrades to
    TTL-only liveness (today's behavior).
    """
    return _session_identity(from_pid)[0]


def resolve_session_harness(from_pid: Optional[int] = None) -> Optional[str]:
    """The harness of this process's nearest harness ancestor, or None.

    The companion of :func:`resolve_session_pid`: one cached verb read answers
    both halves, so the pid and the harness always name the same process (the
    refusing walk that answers the pid declines the whole identity; the
    harness walk behind a pooled spare still proves the harness is claude,
    which thread-worker identity resolution depends on).

    Honors the launcher-stamped proof pair before walking (same rules, applied
    Rust-side): ``FNO_SESSION_PID`` beside ``FNO_SESSION_HARNESS``. The pair is
    stamped by a caller one fork SHALLOWER - the `fno do target init` CLI -
    whose own ppid read was still permitted; under the codex sandbox every
    deeper fork reads PermissionError on its parent's ppid, so the script and
    the verb it spawns cannot walk at all (measured: only self's ppid is
    readable). The pid must be alive, and the name must be a known harness, or
    the stamp is ignored and the walk decides - a stale or forged pair fails
    closed to the walk's own answer.

    Degrades to None when no harness ancestor is found (a plain shell), so a
    caller treats "unproven" and "no ancestor" identically.
    """
    return _session_identity(from_pid)[1]


__all__ = ["pid_dies_with_session", "resolve_session_pid", "resolve_session_harness"]


def _clear_session_identity_cache() -> None:
    """Test seam: drop the per-process memo so a test can re-exec."""
    _session_identity.cache_clear()
