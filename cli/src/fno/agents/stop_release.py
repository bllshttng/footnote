"""Stopping an agent releases the claims it holds (x-9c91 change 5).

The stop itself stays in :mod:`fno.agents.dispatch`; this module adds the
release leg so the oversized dispatch file keeps shrinking. One call to the
native ``release-stopped`` op over the global and repo-local claims dirs.
"""
from __future__ import annotations

import json
import subprocess
from typing import Any, Optional


def stop_agent(name: str, **kwargs: Any):
    """Stop an agent, then release the claims it holds. The codex/gemini
    no-op arm releases nothing, because no stop happened."""
    from fno.agents.dispatch import StopResult, _stop_agent_inner

    result: StopResult = _stop_agent_inner(name, **kwargs)
    if not result.noop:
        release_stopped_claims(name)
    return result


def release_stopped_claims(name: str, session_id: Optional[str] = None) -> None:
    """Release a stopped worker's claims; best-effort, since the stop
    already happened and the reaper is the backstop."""
    from fno.claims.io import claims_dir, global_claims_dir
    from fno.claims.verdict import resolve_binary

    binary = resolve_binary()
    if binary is None:
        print("claims not released: fno-agents binary not found", flush=True)
        return
    if session_id is None:
        # A stop leaves the row so the registry still names its session; an
        # rm already removed it, so the caller passes the id from the lock.
        try:
            from fno.agents.registry import load_registry

            session_id = next(
                (
                    getattr(row, "harness_session_id", None)
                    for row in load_registry()
                    if getattr(row, "name", None) == name
                ),
                None,
            )
        except Exception:  # noqa: BLE001 - best-effort identity
            session_id = None
    command = [str(binary), "claim", "release-stopped", "--name", name]
    if session_id:
        command.extend(("--session", session_id))
    command.extend(("--claims-dir", str(global_claims_dir())))
    command.extend(("--claims-dir", str(claims_dir(None))))
    try:
        proc = subprocess.run(command, capture_output=True, text=True, check=False)
    except OSError as exc:
        print(f"claims not released: {exc}", flush=True)
        return
    if proc.returncode != 0:
        print(
            f"claims not released: {proc.stderr.strip() or f'exit {proc.returncode}'}",
            flush=True,
        )
        return
    try:
        receipt = json.loads(proc.stdout)
    except json.JSONDecodeError:
        return
    released = receipt.get("released") or []
    kept = receipt.get("kept") or []
    if not released and not kept:
        return
    suffix = f"; released {len(released)} claim(s)"
    suffix += "".join(f"; kept {row.get('key')} ({row.get('observed')})" for row in kept)
    print(suffix, flush=True)
