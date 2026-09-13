"""Which codex session does THIS process's rollout prove? (x-a409)

codex holds its rollout fd open and the first-line session_meta carries the id,
so the fd is process ground a leaked env marker cannot forge. The witness here
is what lets a name_only-stamped codex pane complete its own identity: the pane
tree (or, after sibling x-a095 moves pane threads into the shared app-server,
the daemon) holds the rollout, and the witness names every session id that
rollout ground vouches for.

Split out of mux_spawn: that module sits over the file-budget line and may only
shrink; this question deserves its own file.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Mapping, Optional


def _codex_session_ids_for_pid(pid: int, *, psutil_mod=None) -> frozenset:
    """Every distinct codex session id open as a rollout in pid's tree.

    codex holds its rollout fd open and the first-line session_meta carries the
    id, so a process identifies its session deterministically: each pane's tree
    holds a distinct rollout, so a same-cwd sibling can never be mis-identified
    (Codex P1, #603). The pid AND its descendants are inspected: a wrapper
    launcher (the @openai/codex Node shim) holds the pane pid while its native
    child opens the rollout (Codex P1, #603 r5). The id comes from
    session_meta.payload.id, not the filename UUID, which is not always the
    session id in older turn-id layouts (Codex P2, #603 r5).

    Returns an empty set when psutil is unavailable, the process is gone, or no
    rollout is open yet; a tree holding MORE than one distinct session returns
    both (the caller decides whether that is ambiguous).
    """
    psu = psutil_mod
    if psu is None:
        try:
            import psutil
        except ImportError:
            return frozenset()
        psu = psutil
    try:
        procs = [psu.Process(pid)]
    except Exception:  # noqa: BLE001 -- NoSuchProcess / AccessDenied / ZombieProcess
        return frozenset()
    try:
        procs += psu.Process(pid).children(recursive=True)
    except Exception:  # noqa: BLE001 -- a child dying mid-walk yields a partial tree
        pass
    rollout_paths = []
    for proc in procs:
        try:
            files = proc.open_files()
        except Exception:  # noqa: BLE001 -- NoSuchProcess / AccessDenied per proc
            continue
        for f in files:
            base = os.path.basename(f.path)
            if base.startswith("rollout-") and base.endswith(".jsonl"):
                rollout_paths.append(f.path)
    if not rollout_paths:
        return frozenset()
    from fno.agents.discover import _codex_session_meta

    ids = set()
    for path in rollout_paths:
        payload = _codex_session_meta(Path(path))
        sid = payload.get("id") if payload else None
        if isinstance(sid, str) and sid:
            ids.add(sid)
    return frozenset(ids)


def _codex_session_id_for_pid(pid: int, *, psutil_mod=None) -> Optional[str]:
    """The codex TUI's session id from its tree's open rollout, or None.

    One-element wrapper over :func:`_codex_session_ids_for_pid`: a tree holding
    exactly one distinct session returns it; anything else (none, ambiguous)
    returns None.
    """
    ids = _codex_session_ids_for_pid(pid, psutil_mod=psutil_mod)
    if len(ids) == 1:
        return next(iter(ids))
    return None


def codex_rollout_witness(
    harness: str, env: Optional[Mapping[str, str]] = None
) -> frozenset:
    """Session ids a live codex rollout witnesses for THIS process (x-a409).

    Injected into claims.resolve_self_identity as the witness that lets a
    name_only-stamped codex pane complete its own identity: the pane's tree
    holds its rollout fd open, and a leaked marker cannot forge an fd. Two
    oracles, strong first:

    1. The tree scan (:func:`_codex_session_ids_for_pid` on this process's
       resolved pid) - the fd lives in the pane.
    2. The daemon oracle (``fno-agents codex-loaded-threads``), only when the
       tree scan is empty. After sibling x-a095 moves pane threads into the
       shared app-server, the daemon holds the rollout fd and the scan returns
       nothing. The daemon list carries no pid, so this oracle is weaker: it
       requires the present CODEX_THREAD_ID to match a row's session_id AND the
       row's cwd to match this process's cwd AND the match to be unique, and
       witnesses nothing on any other shape (None, zero rows, two rows).

    Returns an empty set for every non-codex harness and every failure; it
    must never raise (identity resolution degrades, never crashes).
    """
    try:
        if harness != "codex":
            return frozenset()
        from fno.claims.session_pid import resolve_session_pid

        pid = resolve_session_pid()
        if pid is not None:
            seen = _codex_session_ids_for_pid(pid)
            if seen:
                return seen
        environ = os.environ if env is None else env
        thread = (environ.get("CODEX_THREAD_ID") or "").strip()
        if not thread:
            return frozenset()
        from fno.agents.discover import _codex_daemon_threads_raw
        from fno.harness_identity import session_identity_key

        rows = _codex_daemon_threads_raw()
        if rows is None:
            return frozenset()
        cwd = os.path.realpath(os.getcwd())
        matches = 0
        for row in rows:
            if not isinstance(row, dict):
                continue
            row_cwd = str(row.get("cwd") or "").strip()
            if not row_cwd or os.path.realpath(row_cwd) != cwd:
                continue
            row_sid = str(row.get("session_id") or "").strip()
            if row_sid and session_identity_key(row_sid) == session_identity_key(thread):
                matches += 1
        if matches == 1:
            return frozenset({thread})
        return frozenset()
    except Exception:  # noqa: BLE001 -- a witness degrades, never crashes
        return frozenset()
