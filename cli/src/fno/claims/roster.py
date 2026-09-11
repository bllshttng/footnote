from __future__ import annotations

from types import MappingProxyType
from typing import Mapping, NamedTuple


class RosterReading(NamedTuple):
    consulted: bool
    rows_scanned: int
    workers_by_node: dict
    reason: str = ""
    rows_by_session: Mapping = MappingProxyType({})
    rows_unresolved: int = 0
    unresolved_rows: tuple = ()
    unmeasurable_by_node: Mapping = MappingProxyType({})

    def workers_on(self, node_id: str) -> list:
        return self.workers_by_node.get(node_id, [])

    def row_for_session(self, session_id: str):
        return self.rows_by_session.get(session_id)


def read_roster(
    timeout: float = 10.0, require_live_probe: bool = True
) -> RosterReading:
    """Read the fleet once and index it by resolved node id.

    ``require_live_probe``: claim-status liveness refuses when the probe
    never ran; the worked overlay passes False (the registry-only view
    still carries attribution).
    """
    try:
        from fno.agents.watchdog import fleet_rows

        rows, warnings = fleet_rows(timeout=timeout)
    except Exception as exc:  # noqa: BLE001 - any failure must degrade loudly
        return RosterReading(False, 0, {}, f"{type(exc).__name__}: {exc}")

    from fno.agents.watchdog import ADVISORY_WARNING_PREFIX, UNMEASURABLE_ROW_PREFIX

    # The harnesses' degraded-probe wording, as a literal: an import would be a layering edge.
    registry_only_mark = "falling back to registry-only view"
    unmeasurable: dict = {}
    blocking = []
    for w in warnings:
        idx = w.find(UNMEASURABLE_ROW_PREFIX)
        if idx == -1:
            degraded_probe = (not require_live_probe) and (registry_only_mark in w)
            if not w.startswith(ADVISORY_WARNING_PREFIX) and not degraded_probe:
                blocking.append(w)
            continue
        fields = dict(
            tok.split("=", 1)
            for tok in w[idx + len(UNMEASURABLE_ROW_PREFIX):].split()
            if "=" in tok
        )
        if fields.get("node"):
            unmeasurable.setdefault(fields["node"], []).append(
                fields.get("name") or "unknown"
            )

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
    return RosterReading(True, len(rows), index, "", by_session, len(unresolved), tuple(unresolved), unmeasurable)


def _finished_row_states() -> frozenset:
    from fno.agents.watchdog import _TERMINAL_STATES, _WAKE_STATES

    return _TERMINAL_STATES - _WAKE_STATES


def _transcript_activity(session_id: str, cwd: str):
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
    if worker.get("state") not in _finished_row_states():
        return False
    return _transcript_activity(worker.get("row_id") or "", worker.get("cwd") or "") is not False
