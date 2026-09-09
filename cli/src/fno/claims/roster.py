"""Shared fleet-roster reading and liveness helpers for claim consumers."""

from __future__ import annotations

from types import MappingProxyType
from typing import Mapping, NamedTuple


class RosterReading(NamedTuple):
    """One reading of the fleet roster, reusable across many claims."""

    consulted: bool
    rows_scanned: int
    workers_by_node: dict
    reason: str = ""
    rows_by_session: Mapping = MappingProxyType({})
    rows_unresolved: int = 0
    unresolved_rows: tuple = ()

    def workers_on(self, node_id: str) -> list:
        return self.workers_by_node.get(node_id, [])

    def row_for_session(self, session_id: str):
        return self.rows_by_session.get(session_id)


def read_roster(timeout: float = 10.0) -> RosterReading:
    """Read the fleet once and index it by resolved node id."""
    try:
        from fno.agents.watchdog import fleet_rows

        rows, warnings = fleet_rows(timeout=timeout)
    except Exception as exc:  # noqa: BLE001 - any failure must degrade loudly
        return RosterReading(False, 0, {}, f"{type(exc).__name__}: {exc}")

    from fno.agents.watchdog import ADVISORY_WARNING_PREFIX

    blocking = [w for w in warnings if not w.startswith(ADVISORY_WARNING_PREFIX)]
    if blocking:
        return RosterReading(False, 0, {}, blocking[0])

    index: dict = {}
    by_session: dict = {}
    unresolved: list[dict] = []
    for r in rows:
        entry = {
            "name": r.name,
            "state": r.state,
            "cwd": r.cwd,
            "row_id": str(r.row_id or ""),
        }
        if r.node:
            index.setdefault(r.node, []).append(entry)
        else:
            unresolved.append(entry)
        if r.row_id:
            by_session[str(r.row_id)] = entry
    return RosterReading(True, len(rows), index, "", by_session, len(unresolved), tuple(unresolved))


def _finished_row_states() -> frozenset:
    """Return the watchdog-derived states that mean a worker stopped."""
    from fno.agents.watchdog import _TERMINAL_STATES, _WAKE_STATES

    return _TERMINAL_STATES - _WAKE_STATES


def _transcript_activity(session_id: str, cwd: str):
    """Return True for finished, False for moving, and None if unreadable."""
    try:
        import time

        from fno.agents.watchdog import (
            QUIET_AFTER_S,
            finished_with_the_tree,
            harness_for_session,
            tail_facts,
        )

        facts = tail_facts(session_id, cwd, agent=harness_for_session(session_id))
        if facts is None:
            return None
        return finished_with_the_tree(facts, time.time(), QUIET_AFTER_S)
    except Exception:  # noqa: BLE001 - an unreadable transcript answers nothing
        return None


def _really_finished(worker: dict) -> bool:
    """Whether a terminal roster row is confirmed by its transcript."""
    if worker.get("state") not in _finished_row_states():
        return False
    return _transcript_activity(worker.get("row_id") or "", worker.get("cwd") or "") is not False
