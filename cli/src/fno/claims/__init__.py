"""fno claim - work-claim coordination primitive.

Public surface for in-process callers (init-target-state.sh, megawalk,
megatron). The CLI is in :mod:`fno.claims.cli`.

Concepts:
    - A claim is a YAML file at .fno/claims/<url-encoded-key>.lock
    - Liveness model: either PID-liveness (default, no expires_at) or TTL
      (expires_at set; refresh() extends)
    - Atomic create via O_CREAT|O_EXCL; the filesystem is the lock
    - Stale recovery: reader sees an existing claim, classifies as
      free|live|stale|corrupted; stale recoveries archive + retry

The six verbs are exposed at module level for direct Python import:
    acquire_claim(key, holder, ...) -> Claim
    release_claim(key, holder, ...) -> None
    refresh_claim(key, holder, ...) -> Claim
    claim_status(key) -> dict
    list_claims(prefix=None, include_stale=False) -> list[dict]
    force_release_claim(key, reason) -> None

Exceptions:
    ClaimAlreadyHeld - low-level: file existed when atomic-create raced
    ClaimHeldByOther - high-level: live claim held by a different holder
    HolderMismatch  - release/refresh with the wrong holder while strict
    ClaimCorrupted  - claim file present but unparseable
    ClaimGoneAway   - file disappeared mid-operation
    ClaimValidationError - invalid input (ttl out of range, key too long, ...)
"""
from __future__ import annotations

from .core import (
    ClaimAlreadyHeld,
    ClaimCorrupted,
    ClaimGoneAway,
    ClaimHeldByOther,
    ClaimValidationError,
    HolderMismatch,
    acquire_claim,
    claim_status,
    force_release_claim,
    list_claims,
    refresh_claim,
    release_claim,
)
from .lanes import (
    DEFAULT_LANE_TTL_MS,
    LANE_HOLDER_PREFIX,
    LANE_SLOT_PREFIX,
    acquire_lane_slot,
    active_lane_count,
    find_lane_slot,
    release_lane_slot,
)
from .types import Claim, ClaimState

__all__ = [
    "Claim",
    "ClaimAlreadyHeld",
    "ClaimCorrupted",
    "ClaimGoneAway",
    "ClaimHeldByOther",
    "ClaimState",
    "ClaimValidationError",
    "DEFAULT_LANE_TTL_MS",
    "HolderMismatch",
    "LANE_HOLDER_PREFIX",
    "LANE_SLOT_PREFIX",
    "acquire_claim",
    "acquire_lane_slot",
    "active_lane_count",
    "claim_status",
    "find_lane_slot",
    "force_release_claim",
    "list_claims",
    "refresh_claim",
    "release_claim",
    "release_lane_slot",
]
