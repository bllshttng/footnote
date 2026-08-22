"""Graph status recomputation and stale-lock detection.

Public API:
    recompute_statuses(entries) -> list[dict]
    compute_readiness(entry, id_to_entry) -> (kind, blocker_id)
    is_stale_lock(task) -> bool
"""
from __future__ import annotations

from datetime import datetime, timezone

from fno.graph._constants import LOCK_TTL_HOURS, PRIORITY_MIGRATION


# Canonical set of derived ``status`` values. Anything else is a typo or
# a stale value that ``recompute_statuses`` should overwrite. Kept here
# (next to the only writer) so the cascade and the validation set live
# together; importers go through this name rather than hard-coding the
# strings at compare sites.
VALID_STATUSES: frozenset[str] = frozenset(
    {"done", "deferred", "superseded", "in_review", "blocked", "in_progress", "idea",
     "design", "ready"}
)

# Legacy `status` values -> current vocabulary. Applied on BOTH the read path
# (`_apply_graph_defaults`, so a not-yet-remutated row reads correctly) and here
# on the write path, mirroring PRIORITY_MIGRATION. `claimed` was renamed to
# `in_progress` so the graph vocabulary matches the lifecycle ladder the rest of
# the system speaks (idea -> design -> ready -> in_progress -> in_review -> done).
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


def compute_readiness(entry: dict, id_to_entry: dict[str, dict]) -> tuple[str, str | None]:
    """Read-time dependency readiness for one entry: never a boolean.

    Returns ("ready", None), ("blocked-by", blocker_id) for a real, open
    blocker, or ("unknown-dep", blocker_id) for a blocked_by id absent from
    the graph. An unknown id always resolves unknown-dep, never ready - fail
    closed on an ambiguous or missing id rather than treat an absence as
    satisfied.

    Call fresh on every read (``_apply_graph_defaults`` is the shared seam
    every reader routes through). Never persisted: ``recompute_statuses``
    does not derive ``status`` from ``blocked_by`` at write time, so this is
    the only place the dependency-satisfaction question gets answered.
    """
    for blocker_id in entry.get("blocked_by") or []:
        blocker = id_to_entry.get(blocker_id)
        if blocker is None:
            return ("unknown-dep", blocker_id)
        if blocker.get("completed_at") is None:
            return ("blocked-by", blocker_id)
    return ("ready", None)


# Lifecycle facts about THIS entry that outrank the blocked overlay: a done,
# shelved, or in-review node is not a dependency question. Kept beside
# compute_readiness so the precedence and the derivation live together.
_OVERLAY_TERMINAL_STATUSES = frozenset({"done", "superseded", "deferred", "in_review"})


def readiness_status(entry: dict, id_to_entry: dict[str, dict]) -> tuple[str | None, str | None]:
    """The one overlay wrapper every status consumer shares.

    Returns ``(status, blocked_reason)`` under the same precedence
    ``_apply_graph_defaults`` applies at read time: terminal statuses pass
    through untouched; everything else overlays ``compute_readiness`` so an
    open blocker reads ``blocked`` with its reason and a ready entry keeps
    its cascade status. The status slot is ``str | None`` because the
    passthrough branches return the row's stored field verbatim, and a
    hand-mangled row may carry no status (or a non-str one) - the wrapper
    derives, it never fabricates a value the row did not have. Both the
    graph read seam (``store._apply_readiness_overlay``) and the parent
    children summaries (``store._compute_children``) call this, so a
    parent's snapshot can never speak a stored ``ready`` that a live read
    derives into ``blocked`` - a second caller-side implementation of the
    precedence is the defect this function exists to make impossible.
    """
    status = entry.get("status")
    # isinstance-first: a hand-mangled unhashable status value ({"nope": 1})
    # must fall through to the non-terminal branch, not raise on the set
    # membership hash. Tuple-`in` tolerated this; a frozenset does not.
    if isinstance(status, str) and status in _OVERLAY_TERMINAL_STATUSES:
        return status, None
    kind, blocker_id = compute_readiness(entry, id_to_entry)
    if kind == "ready":
        return status, None
    return "blocked", f"{kind}:{blocker_id}"


def is_stale_lock(task: dict) -> bool:
    """Check if a feature's claim has expired (>TTL hours)."""
    lock_time_str = task.get("claimed_at")
    # locked_by-first; tolerate a raw legacy (session_id-only) task not yet
    # normalized (read-only staleness check, so no resurrection risk).
    if not (task.get("locked_by") or task.get("session_id")):
        return False
    if not lock_time_str:
        return False
    try:
        lock_time = datetime.fromisoformat(lock_time_str.replace("Z", "+00:00"))
        now = datetime.now(timezone.utc)
        hours_elapsed = (now - lock_time).total_seconds() / 3600
        return hours_elapsed > LOCK_TTL_HOURS
    except (ValueError, TypeError):
        return True  # Unparseable timestamp = treat as stale


def is_open_do_row(row: object) -> bool:
    """Return whether a session row is a valid, unfinished ``do`` window."""
    if not isinstance(row, dict):
        return False
    return (
        row.get("phase") == "do"
        and isinstance(row.get("harness"), str)
        and bool(row["harness"].strip())
        and isinstance(row.get("session_id"), str)
        and bool(row["session_id"].strip())
        and isinstance(row.get("started_at"), str)
        and bool(row["started_at"].strip())
        and "ended_at" not in row
    )


def recompute_statuses(entries: list[dict]) -> list[dict]:
    """Recompute status for all entries based on graph state.

    Called inside locked_mutate_graph() after every mutation.
    Derives status from: completed_at, superseded_by, deferred_at, pr_number,
    locked_by, and open ``do`` session rows. Does NOT derive from blocked_by:
    dependency-satisfaction is a cross-node join that can go stale the instant
    a sibling changes after this write, so it is answered fresh on every read
    instead (see compute_readiness, wired into _apply_graph_defaults in
    store.py) rather than snapshotted here.
    """
    # Reconcile the locked_by/session_id mirror first so derivation keys on the
    # canonical field even when called directly on legacy (session_id-only)
    # entries. Lazy import: store imports this module function-locally too.
    from fno.graph.ladder import plan_rung
    from fno.graph.store import _normalize_lock_fields
    rung_to_status = _rung_to_graph_status()
    _normalize_lock_fields(entries)
    # One-shot priority vocabulary backfill: migrate any legacy
    # high/medium/low values to the new p0/p1/p2/p3 vocabulary the first
    # time each row is touched after the migration ships. Idempotent and
    # self-healing: rows already on the new vocabulary are unaffected.
    for e in entries:
        old_priority = e.get("priority")
        if old_priority in PRIORITY_MIGRATION:
            e["priority"] = PRIORITY_MIGRATION[old_priority]
        old_status = e.get("status")
        if old_status in STATUS_MIGRATION:
            e["status"] = STATUS_MIGRATION[old_status]

    # One-shot defer-vocabulary backfill: pre-feature rows used
    # ``completed_at: "deferred:<ts>"`` to fake deferral. Detect that shape
    # and migrate to the dedicated ``deferred_at`` field so the rest of the
    # cascade and the renderer can rely on a single representation. The
    # prefix never re-appears once migrated, so this is idempotent.
    for e in entries:
        completed = e.get("completed_at")
        if isinstance(completed, str) and completed.startswith(_LEGACY_DEFER_PREFIX):
            e["deferred_at"] = completed[len(_LEGACY_DEFER_PREFIX):]
            e["completed_at"] = None
            e.setdefault("deferred_reason", "")

    for e in entries:
        if not isinstance(e.get("id"), str):
            continue

        # Never persist a stale readiness detail: `_apply_graph_defaults`
        # (store.py) runs this same dict through its read-time blocked
        # overlay before the mutator sees it (locked_mutate_graph reads via
        # _apply_graph_defaults first), which can leave a transient
        # `blocked_reason` on the entry. Reset it unconditionally here so the
        # write path never round-trips that value to disk; the next read
        # recomputes it fresh regardless.
        e["blocked_reason"] = None
        # This marker is recomputed from the row's current lifecycle shape on
        # every write. Clear first so an owner replacement, explicit release,
        # or terminal transition cannot retain an obsolete diagnosis.
        e.pop("ownership_defect", None)

        if e.get("completed_at"):
            e["status"] = "done"
            continue

        # Superseded sits between done and deferred: a node whose work has
        # been fully replaced by another plan should not look ready or
        # deferred. We surface it in its own bucket so the kanban renderer
        # and triage health can show "this is shelved, here is the
        # replacement". Reactivation requires explicit unsupersede (not
        # just undefer) because the user must consciously revive a plan
        # that another plan has already supplanted.
        if e.get("superseded_by"):
            e["status"] = "superseded"
            continue

        # Deferred wins over blocked/claimed/idea/ready. An explicit
        # "do not work on this" signal should not surface as either a
        # ready candidate or a blocked-by graph hint - the LLM and the
        # user both want it in its own bucket.
        if e.get("deferred_at"):
            e["status"] = "deferred"
            continue

        # Reap a stale lock BEFORE the in_review branch: a PR-bearing node with
        # an expired claim (the stampede case) must still shed the dead owner,
        # else `_normalize_lock_fields` later mirrors the stale `locked_by` back
        # into `session_id` at canonicalize/done time and overwrites the
        # merge-time provenance. A persisted in-progress state is different:
        # claimed_at age alone cannot prove the owner died, so preserve that
        # pointer until an explicit repair replaces or clears it.
        if e.get("locked_by") and is_stale_lock(e):
            if e.get("status") == "in_progress":
                e["ownership_defect"] = {
                    "kind": "stale-active-owner-unverified",
                    "node_id": e["id"],
                    "holder": e["locked_by"],
                    "liveness": "unverified",
                }
            else:
                e["locked_by"] = None
                e["session_id"] = None  # keep the one-release mirror in sync
                e["claimed_at"] = None

        # A node carrying a PR that has not closed (merge sets completed_at, so
        # `done` wins above) is IN REVIEW: hold it out of the dispatch pool
        # durably, independent of the builder session's ephemeral PID claim.
        # This promotes the selection-time `_has_unmerged_open_pr` predicate
        # into a persisted status, so the hold is visible to every consumer -
        # explicit named-node dispatch, kanban, triage, `backlog get` - not
        # just the `next`/`ready` candidate filter. Wins over blocked/claimed/
        # idea/ready; defer/supersede/done still win above.
        if e.get("pr_number"):
            e["status"] = "in_review"
            continue

        # Precedence: done > superseded > deferred > in_review > blocked >
        # in_progress > idea > design > ready.
        # Lifecycle states (claim/completion/deferral) win over plan-existence
        # so a plan-less node that gets claimed shows `in_progress`, and a
        # deferred node never re-surfaces. `blocked` is NOT decided here - it
        # is a read-time overlay (compute_readiness, store._apply_graph_defaults)
        # layered on top of whatever this function writes, so a node with an
        # open blocker persists as `in_progress`/idea/design/ready here and
        # reads as `blocked` fresh on every read instead.
        open_do = any(is_open_do_row(row) for row in (e.get("sessions") or []))
        if e.get("locked_by") or open_do:
            e["status"] = "in_progress"
        else:
            # One rung read answers the rest. Persisted so every reader sees it
            # (boards, `backlog get`, the Rust mux).
            #
            # Selection re-probes this value live, but ONLY IN THE DEMOTE
            # DIRECTION: `selection_guards` runs on candidates already filtered
            # to persisted `ready` (graph/cli.py `allowed`), so a row whose doc
            # has since dropped a rung is caught, while a row persisted
            # `design`/`idea` whose doc has since been PROMOTED is filtered out
            # before any live read.
            #
            # Such a row re-arms only when something calls `locked_mutate_graph`
            # - i.e. any unrelated `fno backlog update`. NOT `reconcile`, which
            # reads via `read_graph` and mutates only nodes whose PR has merged,
            # so it never touches a stale rung. Until then the node is invisible
            # to `backlog next`.
            #
            # Spelled out because the previous wording here ("Selection does NOT
            # trust this value - it re-probes the file live") claimed a symmetry
            # that does not exist, and a comment that overstates a guard is the
            # same defect this module was written to remove.
            e["status"] = rung_to_status[plan_rung(e)]

    return entries


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
