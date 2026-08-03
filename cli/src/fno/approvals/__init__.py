"""Approvals and idempotent effects.

Core owns the generalized approval contract and the effect journal. Transports
carry decisions; roles and plugins cannot grant their own authority. One approval
binds one exact principal, action digest, destination, effect class, expiration,
and work-order attempt.

This package produces approval and effect facts. It never declares aggregate
delivery or later business success.
"""

from __future__ import annotations

from fno.approvals.models import (
    DENIED_EFFECT_CLASSES,
    INERT_EFFECT_CLASSES,
    AdapterCapability,
    ApprovalDecision,
    ApprovalRequest,
    Authority,
    DecisionKind,
    EffectAttempt,
    EffectDisposition,
    EffectState,
    PrepareResult,
    ReconciliationRead,
    Refusal,
    RefusalReason,
    RefusedError,
    action_digest,
    canonical_digest,
    classify_effect,
    effect_ref_projection,
    evidence_projection,
)
from fno.approvals.store import EffectStore, default_db_path

__all__ = [
    "AdapterCapability",
    "ApprovalDecision",
    "ApprovalRequest",
    "Authority",
    "DENIED_EFFECT_CLASSES",
    "DecisionKind",
    "EffectAttempt",
    "EffectDisposition",
    "EffectState",
    "EffectStore",
    "INERT_EFFECT_CLASSES",
    "PrepareResult",
    "ReconciliationRead",
    "Refusal",
    "RefusalReason",
    "RefusedError",
    "action_digest",
    "canonical_digest",
    "classify_effect",
    "default_db_path",
    "effect_ref_projection",
    "evidence_projection",
]
