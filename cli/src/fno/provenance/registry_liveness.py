"""Registry-backed liveness for `fno backlog provenance`.

Sits in the provenance package, not the graph package: the graph layer must
not import the agents runtime directly (company boundary, L1 -> L5), and the
provenance package already owns the read-side joins between the two.
"""
from __future__ import annotations

from typing import Optional


def registry_status_index() -> "dict[str, str] | None":
    """Map ``harness_session_id`` -> registry status word for one machine.

    ``None`` means the registry could not be read: the caller then omits
    liveness rather than reading every row as reaped. An empty mapping is a
    real answer, so a row with no entry reads ``reaped``.
    """
    try:
        from fno.agents.registry import load_registry

        rows = load_registry()
    except Exception:  # noqa: BLE001 - liveness must never fail the verb
        return None
    index: dict[str, str] = {}
    for row in rows or []:
        sid = getattr(row, "harness_session_id", None)
        if sid:
            index[sid] = getattr(row, "status", None) or "unknown"
    return index


def registry_status_of(
    session_id: Optional[str], status_index: "dict[str, str] | None"
) -> Optional[str]:
    """The status word one roster row renders, or ``None`` when unannotated."""
    if status_index is None or not session_id:
        return None
    return status_index.get(session_id) or "reaped"
