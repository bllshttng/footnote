"""Which codex session does THIS process's rollout prove? (x-a409)"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Mapping, Optional


def _codex_session_ids_for_pid(pid: int, *, psutil_mod=None) -> frozenset:
    """Every distinct codex session id open as a rollout in pid's tree.

    Each pane's tree holds a distinct rollout fd, so a same-cwd sibling can
    never be mis-identified (Codex P1, #603); the pid and its descendants are
    inspected because a wrapper launcher holds the pane pid while its native
    child opens the rollout. The id is session_meta.payload.id, not the
    filename UUID, which is not always the session id (Codex P2, #603 r5).

    Empty set when psutil is unavailable, the process is gone, or no rollout
    is open yet; more than one distinct session returns all of them.
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
    """The sole session id in pid's tree, or None (none open, or ambiguous)."""
    ids = _codex_session_ids_for_pid(pid, psutil_mod=psutil_mod)
    if len(ids) == 1:
        return next(iter(ids))
    return None


def codex_rollout_witness(
    harness: str, env: Optional[Mapping[str, str]] = None
) -> frozenset:
    """Session ids a live codex rollout witnesses for THIS process (x-a409).

    Injected into claims.resolve_self_identity: a rollout fd is process ground
    a leaked env marker cannot forge. Strong oracle first (the tree scan);
    when it is empty - the post-x-a095 shape, where the shared app-server
    daemon holds the fd - the daemon oracle requires the present
    CODEX_THREAD_ID to match a row's session_id at this process's cwd,
    uniquely. Any other answer, any non-codex harness, any failure: empty set.
    Never raises.
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
