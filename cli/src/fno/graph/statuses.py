"""Graph status recomputation and stale-lock detection.

Public API:
    recompute_statuses(entries) -> list[dict]
    compute_readiness(entry, id_to_entry) -> (kind, blocker_id)
    is_stale_lock(task) -> bool
"""
from __future__ import annotations

from datetime import datetime, timezone

from fno.graph._constants import LOCK_TTL_HOURS


# Canonical set of derived ``status`` values. Anything else is a typo or
# a stale value that ``recompute_statuses`` should overwrite. Kept here
# (next to the only writer) so the cascade and the validation set live
# together; importers go through this name rather than hard-coding the
# strings at compare sites.
VALID_STATUSES: frozenset[str] = frozenset(
    {"done", "deferred", "superseded", "in_review", "blocked", "in_progress", "idea",
     "design", "ready"}
)

# Legacy `status` values -> current vocabulary. The ported store applies it on
# the read AND write paths now (graph_store.rs STATUS_MIGRATION); this table
# stays as the Python-side vocabulary declaration the vocabulary test pins.
# `claimed` was renamed to `in_progress` so the graph vocabulary matches the
# lifecycle ladder the rest of the system speaks (idea -> design -> ready ->
# in_progress -> in_review -> done).
STATUS_MIGRATION: dict[str, str] = {"claimed": "in_progress"}

# The rungs past which a node can never again be dispatched. `done` (work
# shipped) and `superseded` (replaced) are terminal; every other rung is a
# state a node can return from. One spelling shared by the store's
# closure-release hook, the tracker backends' closed set, and the reaper's
# node settlement, so they cannot drift (x-94f8).
TERMINAL_RUNGS: frozenset[str] = frozenset({"done", "superseded"})

# Sentinel prefix used by the pre-feature workaround that overloaded
# ``completed_at`` to encode deferral. Detected once in ``recompute_statuses``
# and migrated to the dedicated ``deferred_at`` field, after which the prefix
# never appears again. Lives here so the migration logic and the parsing logic
# share a single source of truth.
_LEGACY_DEFER_PREFIX = "deferred:"


def is_terminal_entry(entry: object) -> bool:
    """Is this entry closed for good (done/superseded), from its own fields?

    The derived status string plus the two closure signals it keys on, minus
    the legacy ``deferred:`` sentinel: a pre-migration row carries deferral
    inside ``completed_at``, and deferral is a RETURNABLE rung, so a bare
    truthiness check reads it as closed. ``read_graph`` does not run the
    recompute migration, so a raw reader must use this helper, never a bare
    ``completed_at`` test.
    """
    if not isinstance(entry, dict):
        return False
    if entry.get("status") in TERMINAL_RUNGS or entry.get("superseded_by"):
        return True
    completed = entry.get("completed_at")
    return bool(completed) and not str(completed).startswith(_LEGACY_DEFER_PREFIX)


def node_is_done(entry: object) -> bool:
    """The one WORK-done read: the node's stored ruling, nothing else.

    Every Python reader that asks "is this node's work done" calls this
    (x-c672); ``completed_at`` spellings are retired from that question
    (measured 0 divergence in graph and archive). Superseded is deliberately
    absent - a superseded node's WORK is not done, it was replaced, and only
    rebindability reads it that way.
    """
    return isinstance(entry, dict) and entry.get("status") == "done"


def derived_status(entry: object, missing: str = "unknown") -> str:
    """The one status string every reader of a row must agree on.

    A legacy or archived row can carry ``completed_at`` beside a stale open
    status. A reader that takes ``status`` raw then contradicts the row shown
    beside it: a finished dependency renders open inside the Dependencies box,
    a parent rollup counts it open, and the public backlog lists work the
    dashboard calls done.

    Closure is asked of ``is_terminal_entry``, never of a bare ``completed_at``
    truthiness test, which that helper's docstring forbids. The ``deferred:``
    sentinel a pre-migration row hides in ``completed_at`` is a RETURNABLE rung,
    and the render path reads through ``read_graph_with_archive``, which does
    not run the recompute migration, so the sentinel arrives intact.

    The ``completed_at`` term keeps a row terminal by ``status`` alone, carrying
    no timestamp, on its stored status. A ``superseded`` row that DOES carry
    ``completed_at`` reads ``done``, which is long-standing renderer behaviour
    and is left unchanged.

    ``missing`` is the caller's word for an entry that is absent entirely, so a
    relation pointing at an id no longer in the graph keeps saying ``not found``
    rather than ``unknown``.

    Homed here, beside ``is_terminal_entry``, because a copy of this rule per
    renderer is how the dashboard and the public roadmap came to disagree.
    """
    if not isinstance(entry, dict) or not entry:
        return missing
    if is_terminal_entry(entry) and entry.get("completed_at"):
        return "done"
    return str(entry.get("status") or missing)


def _rung_to_graph_status() -> dict:
    """Plan rung -> derived graph status. Total over ``Rung`` by construction.

    Written as a function so the ``ladder`` import stays lazy (``store`` imports
    this module function-locally too), and materialized once on first call.

    Two mappings deserve their reasons stated:

    ``UNREADABLE -> ready`` keeps the node visible and selectable, matching the
    fail-open half of the policy split. Dispatch refuses it separately via
    ``is_dispatchable``, which is where failing closed belongs - encoding the
    refusal in the persisted status would hide the node instead of parking it.

    The plan-side terminals (``IN_PROGRESS``/``IN_REVIEW``/``DONE``/
    ``SUPERSEDED``) also map to ``ready`` rather than to their same-named graph
    statuses. Graph truth for those rungs is ``completed_at`` / ``pr_number`` /
    ``superseded_by``, which the precedence block above already consumed; a plan
    doc must not be able to mark its own node merged by stamping itself.
    """
    from fno.graph.ladder import Rung

    return {
        Rung.NONE: "idea",  # no plan doc at all
        Rung.IDEA: "idea",  # a doc exists but is undesigned (decompose scaffold)
        Rung.DESIGN: "design",
        Rung.READY: "ready",
        Rung.IN_PROGRESS: "ready",
        Rung.IN_REVIEW: "ready",
        Rung.DONE: "ready",
        Rung.SUPERSEDED: "ready",
        Rung.UNREADABLE: "ready",
    }


def lock_timestamp_quality(task: dict) -> str:
    """Classify the graph lock timestamp without deciding owner death."""
    lock_time_str = task.get("locked_at")
    if lock_time_str is None:
        # Direct callers may still hand us a legacy row before the graph read
        # seam has applied its one-write migration.
        lock_time_str = task.get("claimed_at")
    # locked_by-first; tolerate a raw legacy (session_id-only) task not yet
    # normalized (read-only staleness check, so no resurrection risk).
    if not (task.get("locked_by") or task.get("session_id")):
        return "fresh"
    if not lock_time_str:
        return "unreadable"
    try:
        lock_time = datetime.fromisoformat(lock_time_str.replace("Z", "+00:00"))
        now = datetime.now(timezone.utc)
        hours_elapsed = (now - lock_time).total_seconds() / 3600
        return "old" if hours_elapsed > LOCK_TTL_HOURS else "fresh"
    except (ValueError, TypeError):
        return "unreadable"


def is_stale_lock(task: dict) -> bool:
    """Compatibility bool for callers that only need an old/bad timestamp."""
    quality = lock_timestamp_quality(task)
    # Preserve the historical malformed-value result for this compatibility
    # helper. Missing data remains false here; recompute_statuses uses the
    # diagnostic quality directly so both unreadable shapes preserve owners.
    legacy_or_canonical = task.get("locked_at", task.get("claimed_at"))
    return quality == "old" or (quality == "unreadable" and bool(legacy_or_canonical))


def is_open_phase_row(row: object, phase: str) -> bool:
    """Return whether a session row is a valid, unfinished ``phase`` window.

    Same shape as :func:`is_open_do_row` for any lifecycle phase: identified
    (harness + session_id), bounded (started_at), and not yet closed. The do
    flavor additionally drives status derivation; other phases (a spawn-opened
    review row, x-4342) open and close without wedging node status.
    """
    if not isinstance(row, dict):
        return False
    return (
        row.get("phase") == phase
        and isinstance(row.get("harness"), str)
        and bool(row["harness"].strip())
        and isinstance(row.get("session_id"), str)
        and bool(row["session_id"].strip())
        and isinstance(row.get("started_at"), str)
        and bool(row["started_at"].strip())
        and "ended_at" not in row
    )


def is_open_do_row(row: object) -> bool:
    """Return whether a session row is a valid, unfinished ``do`` window."""
    return is_open_phase_row(row, "do")


def recompute_statuses(entries: list[dict]) -> list[dict]:
    """Recompute status for all entries based on graph state.

    The derivation is the ported store's (``graph_store.rs
    recompute_statuses``), answered through the store keeper; this shim keeps
    the in-place, return-the-list contract for the callers that hold entries.
    Derives leaf status from: completed_at, superseded_by, deferred_at,
    pr_number, locked_by, and open ``do`` session rows, then rolls container
    status up from real ``parent`` edges. Does NOT derive from ``blocked_by``:
    dependency-satisfaction is answered fresh on every read instead.
    """
    from fno.graph.store import recompute_statuses_via_store

    recomputed = recompute_statuses_via_store(entries)
    # Write back IN PLACE, both list slots and per-row dicts: callers hold
    # references to individual entry objects and must see the derivation
    # without re-reading the return value.
    for old, new in zip(entries, recomputed):
        if isinstance(old, dict) and isinstance(new, dict):
            old.clear()
            old.update(new)
    entries[:] = recomputed
    return entries




def pending_supersession_reason(entry: dict) -> str | None:
    """Describe a proposed supersession that lacks merged-PR proof."""
    record = entry.get("supersession")
    if not entry.get("superseded_by") or not isinstance(record, dict):
        return None
    if record.get("verified_at"):
        return None
    successor = record.get("successor") or entry.get("superseded_by")
    cause = str(record.get("cause") or "missing cause")
    surfaces = record.get("surfaces") or []
    surface_text = ", ".join(str(s) for s in surfaces) or "missing surfaces"
    return (
        f"pending supersession: successor={successor}; cause={cause}; "
        f"surfaces={surface_text}"
    )


def live_claimed_node_ids(*, strict: bool = False) -> set[str]:
    """Node ids that currently hold a LIVE ``node:<id>`` claim.

    The claim lockfile at ``~/.fno/claims/node:<id>`` is the liveness truth a
    ``/target`` session (or walker-dispatched target) writes; ``classify`` (via
    ``include_stale=False``) filters to only LIVE claims. Homed here — next to
    the ``status`` derivation it complements — so both selection (graph/cli.py)
    and the board renderers can overlay it without a cli<->render import cycle.

    Best-effort by default: any fault in the claims subsystem degrades to an
    empty set so display rendering never breaks on it. Mutation paths pass
    ``strict=True`` and fail closed rather than persisting from fabricated
    no-claim state. Only LIVE claims count; stale/released ones do not.
    """
    try:
        from fno.claims.core import list_claims
        from fno.claims.io import global_claims_root
        live = list_claims(prefix="node:", include_stale=False, root=global_claims_root())
        return {c["key"].removeprefix("node:") for c in live if isinstance(c.get("key"), str)}
    except Exception:
        if strict:
            raise
        return set()
