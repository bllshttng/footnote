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
    {"done", "deferred", "superseded", "blocked", "claimed", "idea", "ready"}
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
    if not task.get("session_id"):
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
    Derives status from: completed_at, blocked_by, session_id.
    """
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

        if e.get("session_id") and is_stale_lock(e):
            e["session_id"] = None
            e["claimed_at"] = None

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

        # Precedence: done > superseded > deferred > blocked > claimed > idea > ready.
        # Lifecycle states (claim/blocker/completion/deferral) win over
        # plan-existence so a plan-less node that gets claimed shows
        # `claimed`, one with an open blocker shows `blocked` rather than
        # `idea`, and a deferred node never re-surfaces in either bucket.
        if has_open_blockers:
            e["_status"] = "blocked"
        elif e.get("session_id"):
            e["_status"] = "claimed"
        elif not e.get("plan_path"):
            # Treat both None and empty string as "no plan" - matches the
            # falsy check in triage._read_plan_excerpt so a graph row that
            # was assigned `plan_path: ""` somewhere doesn't slip into ready.
            e["_status"] = "idea"
        else:
            e["_status"] = "ready"

    return entries
