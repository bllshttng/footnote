"""Shared telemetry helpers for megatron emit sites.

Anchors event writes via :func:`fno.paths.resolve_repo_root` so
telemetry survives invocation from arbitrary cwds. Before this helper
existed, every emit site in :mod:`megatron.state` and :mod:`megatron.queue`
did ``Path(".fno")`` and silently no-op'd whenever the operator
ran ``fno megatron run`` from outside the repo. BUG-MT-008.

The same family bit ``fno event emit`` in PR #270 (Gemini findings:
``feedback_abi_event_emit_subdir_invocation_path_anchoring``); the fix
that landed there did not propagate to the in-process emitters in the
megatron package.
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional


def resolve_events_path() -> Optional[Path]:
    """Return the absolute path to ``.fno/events.jsonl``, anchored.

    Resolution order:
      1. ``resolve_repo_root() / ".fno"`` (the canonical anchor).
      2. ``None`` when no ``.fno/`` is reachable.

    Returning ``None`` lets callers gracefully no-op without leaking the
    error to the state machine that just succeeded. Emit helpers wrap
    their bodies in ``try: ... except Exception: pass`` per the
    best-effort telemetry contract; this helper preserves that contract
    while removing the cwd-anchoring footgun.
    """
    from fno.paths import resolve_repo_root

    abilities_dir = resolve_repo_root() / ".fno"
    if not abilities_dir.is_dir():
        return None
    return abilities_dir / "events.jsonl"
