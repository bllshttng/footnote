"""Graph status recomputation and stale-lock detection.

Public API:
    recompute_statuses(entries) -> list[dict]
    is_stale_lock(task) -> bool
"""
from __future__ import annotations

import sys
from datetime import datetime, timezone

from fno.graph._constants import LOCK_TTL_HOURS, PRIORITY_MIGRATION


# Canonical set of derived ``_status`` values. Anything else is a typo or
# a stale value that ``recompute_statuses`` should overwrite. Kept here
# (next to the only writer) so the cascade and the validation set live
# together; importers go through this name rather than hard-coding the
# strings at compare sites.
VALID_STATUSES: frozenset[str] = frozenset(
    {"done", "deferred", "superseded", "in_review", "blocked", "claimed", "idea", "ready"}
)

# Sentinel prefix used by the pre-feature workaround that overloaded
# ``completed_at`` to encode deferral. Detected once in ``recompute_statuses``
# and migrated to the dedicated ``deferred_at`` field, after which the prefix
# never appears again. Lives here so the migration logic and the parsing logic
# share a single source of truth.
_LEGACY_DEFER_PREFIX = "deferred:"


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


def recompute_statuses(entries: list[dict]) -> list[dict]:
    """Recompute _status for all entries based on graph state.

    Called inside locked_mutate_graph() after every mutation.
    Derives status from: completed_at, superseded_by, deferred_at, pr_number,
    blocked_by, locked_by.
    """
    # Reconcile the locked_by/session_id mirror first so derivation keys on the
    # canonical field even when called directly on legacy (session_id-only)
    # entries. Lazy import: store imports this module function-locally too.
    from fno.graph.store import _normalize_lock_fields
    _normalize_lock_fields(entries)
    # One-shot priority vocabulary backfill: migrate any legacy
    # high/medium/low values to the new p0/p1/p2/p3 vocabulary the first
    # time each row is touched after the migration ships. Idempotent and
    # self-healing: rows already on the new vocabulary are unaffected.
    for e in entries:
        old_priority = e.get("priority")
        if old_priority in PRIORITY_MIGRATION:
            e["priority"] = PRIORITY_MIGRATION[old_priority]

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

    id_to_entry = {e["id"]: e for e in entries if isinstance(e.get("id"), str)}

    for e in entries:
        if not isinstance(e.get("id"), str):
            continue

        if e.get("completed_at"):
            e["_status"] = "done"
            continue

        # Superseded sits between done and deferred: a node whose work has
        # been fully replaced by another plan should not look ready or
        # deferred. We surface it in its own bucket so the kanban renderer
        # and triage health can show "this is shelved, here is the
        # replacement". Reactivation requires explicit unsupersede (not
        # just undefer) because the user must consciously revive a plan
        # that another plan has already supplanted.
        if e.get("superseded_by"):
            e["_status"] = "superseded"
            continue

        # Deferred wins over blocked/claimed/idea/ready. An explicit
        # "do not work on this" signal should not surface as either a
        # ready candidate or a blocked-by graph hint - the LLM and the
        # user both want it in its own bucket.
        if e.get("deferred_at"):
            e["_status"] = "deferred"
            continue

        # Reap a stale lock BEFORE the in_review branch: a PR-bearing node with
        # an expired claim (the stampede case) must still shed the dead owner,
        # else `_normalize_lock_fields` later mirrors the stale `locked_by` back
        # into `session_id` at canonicalize/done time and overwrites the
        # merge-time provenance.
        if e.get("locked_by") and is_stale_lock(e):
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
            e["_status"] = "in_review"
            continue

        has_open_blockers = False
        for blocker_id in e.get("blocked_by", []):
            blocker = id_to_entry.get(blocker_id)
            if blocker is None:
                print(f"Warning: {e['id']} blocked by unknown node {blocker_id}", file=sys.stderr)
                has_open_blockers = True
                break
            if blocker.get("completed_at") is None:
                has_open_blockers = True
                break

        # Precedence: done > superseded > deferred > in_review > blocked > claimed > idea > ready.
        # Lifecycle states (claim/blocker/completion/deferral) win over
        # plan-existence so a plan-less node that gets claimed shows
        # `claimed`, one with an open blocker shows `blocked` rather than
        # `idea`, and a deferred node never re-surfaces in either bucket.
        if has_open_blockers:
            e["_status"] = "blocked"
        elif e.get("locked_by"):
            e["_status"] = "claimed"
        elif not e.get("plan_path"):
            # Treat both None and empty string as "no plan" - matches the
            # falsy check in triage._read_plan_excerpt so a graph row that
            # was assigned `plan_path: ""` somewhere doesn't slip into ready.
            e["_status"] = "idea"
        else:
            e["_status"] = "ready"

    return entries


def live_claimed_node_ids() -> set[str]:
    """Node ids that currently hold a LIVE ``node:<id>`` claim.

    The claim lockfile at ``~/.fno/claims/node:<id>`` is the liveness truth a
    ``/target`` session (or walker-dispatched target) writes; ``classify`` (via
    ``include_stale=False``) filters to only LIVE claims. Homed here — next to
    the ``_status`` derivation it complements — so both selection (graph/cli.py)
    and the board renderers can overlay it without a cli<->render import cycle.

    Best-effort: any fault in the claims subsystem degrades to an empty set so
    neither selection nor rendering ever breaks on it (identical to
    pre-enforcement behavior). Only LIVE claims count; stale/released ones do not.
    """
    try:
        from fno.claims.core import list_claims
        from fno.claims.io import global_claims_root
        live = list_claims(prefix="node:", include_stale=False, root=global_claims_root())
        return {c["key"].removeprefix("node:") for c in live if isinstance(c.get("key"), str)}
    except Exception:
        return set()
