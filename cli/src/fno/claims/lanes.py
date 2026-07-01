"""Lane-slot claims: the atomic concurrency cap for parallel-mode dispatch.

Parallel mode (epic x-42d5, group 1) runs up to ``max_lanes`` background
worktree lanes at once. The cap MUST NOT be a stored integer that dispatch
ticks read-modify-write: two ticks both reading ``count < max`` and both
spawning blows past the cap and defeats the CI-cost bound (design Locked
Decision #7).

Instead the cap is enforced by claim ATOMICITY. There are exactly
``max_lanes`` fixed slot keys, ``lane-slot:0`` .. ``lane-slot:<max_lanes-1>``.
Acquiring a lane means atomically grabbing the first free slot via the O_EXCL
claim primitive. When every slot is held by a live lane, no slot is free and
acquisition returns ``None`` - the filesystem, not a counter, is the cap.
``active_lane_count()`` is DERIVED from the live slot claims for observability
(status rollups, US5); it is never the enforcement gate. Do NOT "simplify"
acquisition into a count-then-acquire check - that reintroduces exactly the
race this design avoids.

Slot claims are repo-local coordination state (like ``walker:<root>``): the
``lane-slot:`` prefix is not a global-id prefix, so ``claims_root_for`` routes
it to the canonical repo's ``.fno/claims`` and every worktree lane of the
project shares one cap.

Lane identity vs holder: each lane passes a unique ``lane_id`` (typically the
backlog node id). The claim holder is derived as ``parallel-lane:<lane_id>`` so
(a) two distinct lanes never collide onto one slot, and (b) a re-dispatch of
the SAME lane idempotently re-takes its OWN slot rather than inflating the cap.
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

from .core import (
    ClaimHeldByOther,
    ClaimValidationError,
    acquire_claim,
    list_claims,
    release_claim,
)
from .types import Claim

# Slot key namespace. Not a global-id prefix (see claims.io._GLOBAL_ID_PREFIXES),
# so lane slots stay repo-local and coordinate across the project's worktrees.
LANE_SLOT_PREFIX = "lane-slot:"
LANE_HOLDER_PREFIX = "parallel-lane:"

# A lane can outlive the dispatcher tick that spawned it (spawn -> target init
# -> a multi-phase /target run), so lane slots are TTL-anchored and refreshed by
# the owner while the lane is alive, not pinned to a transient dispatcher PID.
DEFAULT_LANE_TTL_MS = 3_600_000  # 1 hour; the owner refreshes for longer lanes


def _slot_key(index: int) -> str:
    return f"{LANE_SLOT_PREFIX}{index}"


def _lane_holder(lane_id: str) -> str:
    return f"{LANE_HOLDER_PREFIX}{lane_id}"


def find_lane_slot(lane_id: str, *, root: Optional[Path] = None) -> Optional[str]:
    """Return the slot key this lane already holds, or None.

    Scans all live ``lane-slot:`` claims for one whose holder matches this
    lane. Index-agnostic on purpose: a slot at any index (including one above a
    shrunken ``max_lanes``) is still found and reused, so a lane never ends up
    holding two slots.
    """
    holder = _lane_holder(lane_id)
    for claim in list_claims(prefix=LANE_SLOT_PREFIX, root=root):
        if claim.get("holder") == holder:
            return claim["key"]
    return None


def acquire_lane_slot(
    max_lanes: int,
    lane_id: str,
    *,
    ttl_ms: Optional[int] = DEFAULT_LANE_TTL_MS,
    reason: Optional[str] = None,
    root: Optional[Path] = None,
) -> Optional[Claim]:
    """Atomically acquire one of ``max_lanes`` fixed lane slots for ``lane_id``.

    Returns the acquired :class:`Claim`, or ``None`` when every slot is held by
    a live lane (the cap is full). Enforcement is the atomic O_EXCL grab, NOT a
    pre-read count (Locked Decision #7).

    ``max_lanes`` must be >= 1; ``max_lanes == 1`` degrades to a single-slot
    lock, i.e. today's sequential behavior, with no special-case code.

    Lane-level idempotency: if this lane already owns a live slot it is reused
    (refreshed) rather than a second one grabbed - so a re-dispatch of the same
    lane cannot inflate the cap even after an earlier-indexed slot has freed.
    """
    if not lane_id:
        raise ClaimValidationError("lane_id must be non-empty")
    if max_lanes < 1:
        raise ClaimValidationError(f"max_lanes must be >= 1, got {max_lanes}")

    # A lane slot is ALWAYS TTL-anchored, never PID-liveness. An explicit None
    # (e.g. CLI `--ttl ""`) must NOT fall through to acquire_claim's default,
    # which would pin the slot to the transient acquiring process - a one-shot
    # `fno claim lane-acquire` exits immediately, the PID dies, the slot goes
    # instantly stale, and the cap stops being enforced. Coerce to the lane
    # default instead.
    if ttl_ms is None:
        ttl_ms = DEFAULT_LANE_TTL_MS

    holder = _lane_holder(lane_id)
    metadata = {"lane_id": lane_id}

    # Reuse this lane's existing slot if it already holds one (idempotent at
    # lane granularity). This is safe under the realistic access pattern:
    # concurrent acquisitions carry DISTINCT lane_ids (distinct holders), and
    # repeat acquisitions of one lane_id are sequential dispatcher retries.
    existing = find_lane_slot(lane_id, root=root)
    if existing is not None:
        return acquire_claim(
            key=existing,
            holder=holder,
            ttl_ms=ttl_ms,
            reason=reason,
            metadata=metadata,
            root=root,
        )

    for i in range(max_lanes):
        try:
            return acquire_claim(
                key=_slot_key(i),
                holder=holder,
                ttl_ms=ttl_ms,
                reason=reason,
                metadata=metadata,
                root=root,
            )
        except ClaimHeldByOther:
            continue  # slot held by a live lane; try the next
    return None  # cap full: every slot is held


def release_lane_slot(lane_id: str, *, root: Optional[Path] = None) -> None:
    """Release the slot held by ``lane_id``. Silent no-op if it holds none."""
    slot = find_lane_slot(lane_id, root=root)
    if slot is None:
        return
    release_claim(key=slot, holder=_lane_holder(lane_id), root=root)


def active_lane_count(*, root: Optional[Path] = None) -> int:
    """Count live lane slots. DERIVED observability value, never the cap gate.

    ``list_claims`` returns live claims only, so a crashed lane whose TTL has
    lapsed does not count against the cap and its slot is reclaimable.
    """
    return len(list_claims(prefix=LANE_SLOT_PREFIX, root=root))


__all__ = [
    "LANE_SLOT_PREFIX",
    "LANE_HOLDER_PREFIX",
    "DEFAULT_LANE_TTL_MS",
    "acquire_lane_slot",
    "release_lane_slot",
    "active_lane_count",
    "find_lane_slot",
]
