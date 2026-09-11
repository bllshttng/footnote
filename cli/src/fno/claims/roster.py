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
            "pid": getattr(r, "pid", None),
            "pid_start_time": getattr(r, "pid_start_time", None),
            "mux": getattr(r, "mux", None),
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


def classify_workers(workers: list) -> tuple[list, list, dict]:
    """(engaged, unmeasurable, verdicts-by-name) for one roster read."""
    from fno.agents.reachability import REACHABLE, UNKNOWN

    verdicts = {w.get("name") or "": _worker_reachability(w).verdict for w in workers}
    return (
        [w for w in workers if verdicts.get(w.get("name") or "") == REACHABLE],
        [w for w in workers if verdicts.get(w.get("name") or "") == UNKNOWN],
        verdicts,
    )


def _worker_reachability(worker: dict):
    """One roster row through the ONE shared predicate (x-dead task 1.1).

    REACHABLE engaged, UNREACHABLE finished, UNKNOWN its own arm - never
    engaged-by-default. The transcript outranks the supervisor word for EVERY
    row (a finished worker's row never leaves `working`); a terminal word
    with no transcript stays positive evidence of the end.
    """
    from fno.agents.reachability import (
        TRANSCRIPT_EVIDENCE_S,
        classify_reachability,
        pane_falsifier,
        pid_falsifier,
    )

    state = worker.get("state")
    try:
        import time

        from fno.agents.watchdog import classify_tail, harness_for_session, tail_facts

        facts = tail_facts(
            worker.get("row_id") or "", worker.get("cwd") or "",
            agent=harness_for_session(worker.get("row_id") or ""),
        )
    except Exception:  # noqa: BLE001 - an unreadable transcript answers nothing
        facts = None
    # The pid falsifier fires only on an INCARNATION-PROVEN pid (a recorded
    # start token, the same bound the claims layer's hybrid arm uses): a bare
    # stale pid on a live worker must never read as death. A FRESH tail is a
    # resumed session's witness (x-a613) over even a proven corpse.
    pid = worker.get("pid")
    proven = pid is not None and worker.get("pid_start_time") is not None
    falsifier = pid_falsifier(pid, worker.get("pid_start_time")) if proven else None
    if falsifier is None:
        falsifier = pane_falsifier(worker.get("mux"))
    if facts is None:
        # No transcript: the supervisor word is the only evidence. An active
        # word stays reachable; a terminal word positively ended the row.
        if falsifier is not None:
            return classify_reachability(truth_state=None, age_s=None, falsifier=falsifier)
        if state in ("working", "watching", "your-move"):
            return classify_reachability(truth_state=state, age_s=None, falsifier=None)
        finished = f"finished-state:{state}" if state in _finished_row_states() else None
        return classify_reachability(truth_state=None, age_s=None, falsifier=finished)
    if facts.last_event_epoch is None:
        # A transcript PRESENT but undatable: the measured wrong answer read
        # this as engaged. It is UNKNOWN - a verdict about the instrument -
        # never engaged-by-default and never positively finished.
        return classify_reachability(truth_state=None, age_s=None, falsifier=falsifier)
    age = int(max(0.0, time.time() - facts.last_event_epoch))
    if falsifier is not None and age <= TRANSCRIPT_EVIDENCE_S:
        falsifier = None
    return classify_reachability(
        truth_state=classify_tail(facts.last_role, facts.last_text, age),
        age_s=age,
        falsifier=falsifier,
    )
