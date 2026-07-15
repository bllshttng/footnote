"""Resolve a durable session pid for the hybrid liveness pid-arm (ab-cc5553f2).

The ``node:<id>`` claim is acquired with ``--ttl`` AND ``--pid <durable>``. The
durable pid must be the process that lives as long as the *session*, not the
transient ``fno`` python subprocess that runs ``fno target init`` (that pid is
dead ~1s after init returns - the original bug). Every agent harness runs its
session under a long-lived binary (``claude``, ``codex``, ``gemini``,
``opencode``, ``agy``), so the uniform mechanism is a process-tree walk from
init up the parent chain to the nearest *harness* ancestor.

This is degrade-safe by construction: if no harness ancestor is found (e.g.
plain-shell), the caller records no ``--pid`` (or the transient default) and the
claim is LIVE via the TTL arm exactly as before. A mis-resolved/transient pid is
a dead pid that fails ``is_live`` -> STALE on expiry, indistinguishable from a
missing one. The pid arm only ever *extends* liveness (every matched process is
an ancestor of the acquiring one, so it dies no later than the session), so it
is structurally impossible to regress.
"""
from __future__ import annotations

import os
from typing import Iterator, Optional

import psutil

# How far up the parent chain to look before giving up. The real chain is
# short (claude -> ... -> fno -> bash init), but bg/handoff nesting can add a
# few levels; 25 is generous and bounds a pathological/looping ancestry.
_MAX_DEPTH = 25

# Harness session binaries whose ancestor anchors the durable pid. Keep in sync
# with spawn's KNOWN_PROVIDERS (the sibling harness list); no runtime import -
# claims sits at the bottom of the stack and must not couple to the spawn
# registry.
_HARNESS_TOKENS = ("claude", "codex", "gemini", "opencode", "agy")
# `claude` keeps its proven substring rule (unchanged: its versioned binary
# hides the name in the exe path, and the shipped x-616b lane depends on it).
# The rest match by exact path SEGMENT only, never substring: `agy` is a
# substring of `legacy`, and the ChatGPT desktop app's process tree is full of
# `Codex Framework.framework` exe paths whose segments are not `codex`.
_SEGMENT_TOKENS = frozenset(t for t in _HARNESS_TOKENS if t != "claude")

_PSUTIL_ERRORS = (psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess)


def _candidate_strings(proc: psutil.Process) -> Iterator[tuple[str, bool]]:
    """Yield ``(identity_string, is_cmdline)`` for PROC: ``name()``, ``exe()``
    (``is_cmdline=False``), then the first two ``cmdline()`` entries
    (``is_cmdline=True``). cmdline is what covers node-shim harnesses (gemini is
    ``node /.../bin/gemini`` behind a symlink, so name/exe say only ``node``).
    Each getter's psutil failure is skipped independently - a per-getter error is
    "no evidence", not a dead chain.
    """
    for getter in (proc.name, proc.exe):
        try:
            value = getter()
        except _PSUTIL_ERRORS:
            continue
        if value:
            yield value, False
    try:
        argv = proc.cmdline()
    except _PSUTIL_ERRORS:
        argv = []
    for value in argv[:2]:
        if value:
            yield value, True


def _matches_harness(proc: psutil.Process) -> bool:
    """True iff PROC is a recognized harness session binary."""
    for cand, is_cmdline in _candidate_strings(proc):
        low = cand.lower()
        # claude: substring match, but NEVER against a cmdline entry. argv carries
        # full paths that often contain a `.claude/` install segment - a wrapper
        # `bash ~/.claude/plugins/fno/hooks/.../init-target-state.sh` is an
        # ancestor of `fno claim session-pid`, and a substring test there would
        # match that transient shell and return its short-lived pid instead of the
        # real long-lived claude process (codex P1, PR #419). name/exe of such a
        # wrapper are just `bash`/`/bin/bash`, so the substring rule stays sound.
        if not is_cmdline and "claude" in low:
            return True
        for seg in low.split("/"):
            # Segment-exact tokens are safe on argv too: a path segment equals a
            # token only when it IS the harness binary. Compare the segment and
            # its extension-stripped stem, so both `bin/gemini` and
            # `bundle/gemini.js` match `gemini`.
            if seg in _SEGMENT_TOKENS or seg.rsplit(".", 1)[0] in _SEGMENT_TOKENS:
                return True
    return False


def resolve_session_pid(from_pid: Optional[int] = None) -> Optional[int]:
    """Return the durable session pid, or None if uncapturable (degrade).

    Resolution order:
      1. ``FNO_SESSION_PID`` env, if set to a live pid (launcher override).
      2. The nearest harness ancestor of FROM_PID (default: this process's
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
    # harness is always the first hit in that chain, so a binary that merely
    # lives under a `.claude/` path elsewhere cannot yield a wrong pid. First
    # match wins, so a harness nested under another (a claude worker spawned by
    # codex) anchors to its nearest session, not the outermost one.
    start = from_pid if from_pid is not None else os.getppid()
    try:
        proc: Optional[psutil.Process] = psutil.Process(start)
    except (psutil.NoSuchProcess, psutil.AccessDenied, ValueError):
        # ValueError covers a negative/zero start pid; all degrade to None.
        return None

    depth = 0
    while proc is not None and depth < _MAX_DEPTH:
        if _matches_harness(proc):
            return proc.pid
        try:
            proc = proc.parent()
        except _PSUTIL_ERRORS:
            return None
        depth += 1
    return None


__all__ = ["resolve_session_pid"]
