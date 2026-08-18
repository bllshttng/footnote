"""The cadence-deadline backstop: silence becomes a positive finding.

The refusal path (``recovery.classify_worker_refusal``) catches a worker whose
own last turn carries a marker the taxonomy recognises. This catches the rest:
a worker that stopped producing turns and said nothing about why. A harness can
reword its refusal tomorrow, and a cap can arrive as a hang rather than a
sentence, so the taxonomy alone leaves a gap that only a clock closes.

Three constraints, all load-bearing.

**It reads the FULL registry, not recovery's candidate set.** ``iter_candidates``
drops every row whose harness is not claude and every row without a live bg
messaging socket, so the moment failover spawns a codex successor, that
successor is invisible to the sweep that would catch its cap.

**It never acts.** No stop, no spawn, no claim mutation, no row write. A
component that reports a wrong reading is a false alarm; a component that acts
on the same wrong reading is lost work, and every measured failure in this
fleet was a case where the ROSTER was wrong. Silence is a report and acts
never; a refusal is affirmative evidence and acts immediately. That asymmetry
is the whole design.

**An unknowable age emits nothing.** ``resolve_session_truth`` returns None for
age when a session keeps no per-file transcript (an opencode DB), and absence
of evidence must not become a finding.
"""

from __future__ import annotations

import dataclasses
import time
from typing import Any, Callable, Optional

#: Default cadence deadline. Borrowed from ``session_truth.STALE_ATTENTION_S``,
#: which already exists as the attention window and carries an explicit note
#: that no verdict or reap decision keys off it - exactly the right pedigree for
#: a value that must inform and never act.
DEFAULT_SILENCE_DEADLINE_S = 600

#: Wall-clock budget for one sweep, in seconds. Resolving a worker's transcript
#: truth means locating its session and reading a tail, and a large roster takes
#: MINUTES of that. This sweep runs on the pr-watch tick, which is bounded by a
#: SIGALRM: an unbounded read here would blow the very deadline this node moved
#: the fleet leg in front of. Rows past the budget are reported as unread, which
#: makes their age unknowable, which emits nothing - the same honest answer as a
#: transcript that cannot be read at all.
DEFAULT_SWEEP_BUDGET_S = 20.0


@dataclasses.dataclass(frozen=True)
class SweepRow:
    """One worker's verdict. Healthy rows are included on purpose.

    ``--json`` prints every row, so an empty or shapeless answer cannot pass a
    gate that only ever greps for a key on the unhappy path.
    """

    handle: str
    harness: str
    deadline_s: int
    age_s: Optional[int] = None
    silent: bool = False
    node: Optional[str] = None
    last_message: Optional[str] = None
    state: Optional[str] = None
    #: True when the sweep ran out of budget before reading this row. It is
    #: named rather than dropped: a row silently missing from the answer reads
    #: as a fleet with fewer workers, and a truncation nobody can see is the
    #: same lie as a silent cap.
    unread: bool = False

    def as_dict(self) -> dict[str, Any]:
        return dataclasses.asdict(self)


def _node_of(cwd: str) -> Optional[str]:
    from fno.recovery import _node_id_from_worktree

    try:
        return _node_id_from_worktree(cwd) if cwd else None
    except Exception:  # noqa: BLE001 - a manifest miss is not a finding
        return None


def sweep_rows(
    *,
    deadline_s: Optional[int] = None,
    registry_load: Optional[Callable[[], list]] = None,
    truth_fn: Optional[Callable[[Any], dict]] = None,
    now_s: Optional[float] = None,
    budget_s: float = DEFAULT_SWEEP_BUDGET_S,
) -> list[SweepRow]:
    """One verdict per registered worker. Reads only; writes nothing.

    Every I/O seam is injected so this is unit-testable offline, matching
    ``recovery.recovery_sweep``'s own shape.

    ``budget_s`` bounds the wall clock. Rows the budget did not reach are
    returned marked ``unread`` with no age, so they emit nothing and are still
    visible in the answer. Dropping them instead would render a truncated sweep
    as a smaller fleet.
    """
    if deadline_s is None:
        deadline_s = _configured_deadline()
    if registry_load is None:
        from fno.agents.registry import load_registry

        registry_load = load_registry
    if truth_fn is None:
        from fno.agents.session_truth import resolve_session_truth

        def truth_fn(entry):  # type: ignore[misc]
            # Resolve the row to ITSELF rather than letting the default
            # resolver re-discover it. The discovery scan runs per row and
            # costs about ten seconds each on a real roster, which would eat
            # the whole tick deadline this node moved the fleet leg in front
            # of. Same seam recovery.py uses for the same reason.
            return resolve_session_truth(
                entry.name, resolve=lambda _h: (entry, []), now_s=now_s,
            )

    try:
        entries = registry_load()
    except Exception:  # noqa: BLE001 - an unreadable registry reports nothing
        return []

    rows: list[SweepRow] = []
    started = time.monotonic()
    for entry in entries:
        if budget_s > 0 and time.monotonic() - started > budget_s:
            rows.append(SweepRow(
                handle=entry.name,
                harness=getattr(entry, "harness", "") or "",
                deadline_s=int(deadline_s),
                unread=True,
                state="not-read",
            ))
            continue
        try:
            truth = truth_fn(entry)
        except Exception:  # noqa: BLE001 - one bad row never aborts the sweep
            truth = {}
        raw_age = truth.get("last_activity_age_s")
        age = int(raw_age) if isinstance(raw_age, (int, float)) else None
        rows.append(SweepRow(
            handle=entry.name,
            harness=getattr(entry, "harness", "") or "",
            deadline_s=int(deadline_s),
            age_s=age,
            # An unknowable age is never silent. `age is None` means the
            # instrument did not read, not that the worker was quiet, and those
            # two have to stay distinguishable.
            silent=age is not None and age > deadline_s,
            node=_node_of(getattr(entry, "cwd", "") or ""),
            last_message=truth.get("last_message"),
            state=truth.get("state"),
        ))
    return rows


def _configured_deadline() -> int:
    try:
        from fno.config import load_settings

        value = getattr(
            load_settings().agents, "silence_deadline_seconds",
            DEFAULT_SILENCE_DEADLINE_S,
        )
        return int(value) if int(value) > 0 else DEFAULT_SILENCE_DEADLINE_S
    except Exception:  # noqa: BLE001 - a config miss uses the built-in window
        return DEFAULT_SILENCE_DEADLINE_S


def run_sweep(
    *,
    emit: Optional[Callable[[str, dict], None]] = None,
    deadline_s: Optional[int] = None,
    registry_load: Optional[Callable[[], list]] = None,
    truth_fn: Optional[Callable[[Any], dict]] = None,
    now_s: Optional[float] = None,
    budget_s: float = DEFAULT_SWEEP_BUDGET_S,
    source: str = "daemon",
    dedup: bool = True,
) -> tuple[list[SweepRow], int]:
    """``(rows, silent_count)``. Emits ``worker_silent`` on the TRANSITION.

    The event is the producer the watching agent later reads. Nothing here
    consumes it, and nothing here acts on it.

    ``dedup`` keeps the emit once per silent spell rather than once per tick.
    A worker idle past its deadline stays idle for hours, so a per-tick emit
    would be six events an hour per worker forever, and an event that fires
    every tick is not a "something changed" signal at all. A handle that goes
    quiet, comes back, and goes quiet again is two findings, because it drops
    out of the seen set the moment it reports healthy.

    ``source`` names who ran it, so a hand-run report is not filed as a daemon
    observation.
    """
    rows = sweep_rows(
        deadline_s=deadline_s, registry_load=registry_load,
        truth_fn=truth_fn, now_s=now_s, budget_s=budget_s,
    )
    emitter = emit if emit is not None else _emitter_for(source)

    seen: set = set()
    if dedup:
        try:
            from fno import fleet_state

            seen = fleet_state.silent_seen()
        except Exception:  # noqa: BLE001 - no memory means report, never suppress
            seen = set()

    silent = 0
    now_silent = set()
    for row in rows:
        if not row.silent:
            continue
        silent += 1
        now_silent.add(row.handle)
        if row.handle in seen:
            continue
        emitter("worker_silent", {
            "handle": row.handle,
            "harness": row.harness,
            "age_s": row.age_s,
            "deadline_s": row.deadline_s,
            "node": row.node,
            "last_message": row.last_message,
        })

    if dedup:
        try:
            from fno import fleet_state

            # Only rows the sweep actually READ can leave the set. An unread
            # row (past the budget) keeps its memo, or a truncated sweep would
            # re-report every worker it did not reach next tick.
            unread = {r.handle for r in rows if r.unread}
            fleet_state.set_silent_seen(now_silent | (seen & unread))
        except Exception:  # noqa: BLE001 - a lost memo costs one repeat report
            pass
    return rows, silent


def _emitter_for(source: str) -> Callable[[str, dict], None]:
    """Best-effort canonical emit; a lost event never breaks a read-only sweep."""

    def _emit(event_type: str, data: dict) -> None:
        try:
            from fno.events import _build, append_event
            from fno.paths import state_dir

            append_event(
                _build(event_type, source, data), state_dir() / "events.jsonl"
            )
        except Exception:  # noqa: BLE001
            pass

    return _emit
