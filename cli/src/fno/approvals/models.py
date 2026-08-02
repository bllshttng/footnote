"""Immutable records that bind one approval to one exact external effect.

Approval is not execution, execution is not acknowledgment, and acknowledgment
is not an aggregate delivery verdict. Each of those is a separate record here so
no consumer can collapse them.

Authority lives outside this module. A role, plugin, transport, or CLI caller can
carry a decision but never widens who may approve: the store consults an
:class:`Authority` supplied by independent policy, and nothing in a decision
payload reaches that lookup.
"""

from __future__ import annotations

import datetime as _dt
import hashlib
import json
from collections.abc import Mapping
from enum import Enum
from typing import Any, Protocol, Self, runtime_checkable

from pydantic import BaseModel, ConfigDict, model_validator

from fno.company.contracts import (
    EffectRef,
    EvidenceRef,
    EvidenceResult,
    EvidenceSubjectKind,
    NonEmptyStr,
)

__all__ = [
    "ApprovalDecision",
    "ApprovalRequest",
    "AdapterCapability",
    "Authority",
    "DecisionKind",
    "DENIED_EFFECT_CLASSES",
    "EffectAttempt",
    "EffectDisposition",
    "EffectState",
    "INERT_EFFECT_CLASSES",
    "PrepareResult",
    "Refusal",
    "RefusalReason",
    "RefusedError",
    "action_digest",
    "canonical_digest",
    "classify_effect",
    "effect_ref_projection",
    "evidence_projection",
    "utcnow",
]


def utcnow() -> _dt.datetime:
    return _dt.datetime.now(_dt.timezone.utc)


def canonical_digest(payload: Mapping[str, Any]) -> str:
    """Digest a mapping so any change to any bound field changes the digest."""
    encoded = json.dumps(
        payload, sort_keys=True, separators=(",", ":"), ensure_ascii=False, default=str
    )
    return hashlib.sha256(encoded.encode("utf-8")).hexdigest()


def action_digest(content: Mapping[str, Any]) -> str:
    """Digest the action's content. Editing the content invalidates its approval."""
    return canonical_digest(content)


class EffectState(str, Enum):
    PREPARED = "prepared"
    EXECUTING = "executing"
    ACKNOWLEDGED = "acknowledged"
    FAILED = "failed"
    BLOCKED = "blocked"
    UNKNOWN = "unknown"


#: States that may never be re-dispatched or re-settled.
TERMINAL_STATES = frozenset({EffectState.ACKNOWLEDGED, EffectState.BLOCKED})


class DecisionKind(str, Enum):
    APPROVED = "approved"
    DECLINED = "declined"


class EffectDisposition(str, Enum):
    ALLOW = "allow"
    REQUIRE_APPROVAL = "require_approval"
    DENY = "deny"


#: Refused until a later explicit policy and adapter contract exist.
DENIED_EFFECT_CLASSES: frozenset[str] = frozenset(
    {
        "financial.payment",
        "financial.commitment",
        "signature.contract",
        "employment.action",
        "infrastructure.destructive",
    }
)

#: No external consequence, so no effect approval. Independent policy may still
#: declare a consequential destination, which is why these are classes and not
#: an escape hatch keyed on the caller.
INERT_EFFECT_CLASSES: frozenset[str] = frozenset(
    {
        "internal.draft",
        "internal.research",
    }
)


def classify_effect(effect_class: str) -> EffectDisposition:
    """Classify an effect class. Function-agnostic: only the class is read.

    Unrecognised classes require approval rather than sliding through, so a new
    effect class is safe by default instead of silently exempt.
    """
    if effect_class in DENIED_EFFECT_CLASSES:
        return EffectDisposition.DENY
    if effect_class in INERT_EFFECT_CLASSES:
        return EffectDisposition.ALLOW
    return EffectDisposition.REQUIRE_APPROVAL


class RefusalReason(str, Enum):
    UNAUTHORIZED_PRINCIPAL = "unauthorized_principal"
    UNKNOWN_REQUEST = "unknown_request"
    DIGEST_MISMATCH = "digest_mismatch"
    EXPIRED = "expired"
    DECLINED = "declined"
    NOT_APPROVED = "not_approved"
    REPLAY = "replay"
    CONFLICTING_BINDING = "conflicting_binding"
    DENIED_EFFECT_CLASS = "denied_effect_class"
    UNSAFE_RETRY = "unsafe_retry"
    TERMINAL_STATE = "terminal_state"


@runtime_checkable
class Authority(Protocol):
    """Independent policy source. Implemented outside this package."""

    #: Human-readable identity of the policy consulted, quoted back in refusals.
    source: str

    def may_approve(self, *, principal_id: str, effect_class: str, destination: str) -> bool:
        """Return True only if this principal may approve this exact effect shape."""


class _ApprovalModel(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class Refusal(_ApprovalModel):
    """A typed, stable refusal. Never carries an approved receipt."""

    reason: RefusalReason
    detail: NonEmptyStr
    fields: tuple[str, ...] = ()
    authority_source: str | None = None
    recovery: str | None = None


class RefusedError(Exception):
    """Raised instead of returned so an ignored result cannot become a dispatch."""

    def __init__(self, refusal: Refusal) -> None:
        super().__init__(f"{refusal.reason.value}: {refusal.detail}")
        self.refusal = refusal


def _require_utc(value: _dt.datetime, field: str) -> None:
    if value.tzinfo is None or value.utcoffset() is None:
        raise ValueError(f"{field} must be timezone-aware UTC")


class ApprovalRequest(_ApprovalModel):
    """One exact effect awaiting one exact decision."""

    request_id: NonEmptyStr
    principal_id: NonEmptyStr
    work_order_id: NonEmptyStr
    attempt_id: NonEmptyStr
    effect_id: NonEmptyStr
    effect_class: NonEmptyStr
    destination: NonEmptyStr
    action_digest: NonEmptyStr
    created_at: _dt.datetime
    expires_at: _dt.datetime

    @model_validator(mode="after")
    def _validate_window(self) -> Self:
        _require_utc(self.created_at, "created_at")
        _require_utc(self.expires_at, "expires_at")
        if self.expires_at <= self.created_at:
            raise ValueError("expires_at must be after created_at")
        return self

    @property
    def bound_fields(self) -> dict[str, str]:
        """Exactly the fields a decision is bound to."""
        return {
            "principal_id": self.principal_id,
            "work_order_id": self.work_order_id,
            "attempt_id": self.attempt_id,
            "effect_id": self.effect_id,
            "effect_class": self.effect_class,
            "destination": self.destination,
            "action_digest": self.action_digest,
            "expires_at": self.expires_at.isoformat(),
        }

    @property
    def request_digest(self) -> str:
        return canonical_digest(self.bound_fields)

    def is_expired(self, now: _dt.datetime) -> bool:
        return now >= self.expires_at


class ApprovalDecision(_ApprovalModel):
    """An immutable decision on one request digest.

    ``transport`` records where the decision arrived from. It is informational:
    the store authorizes ``deciding_principal_id`` against independent policy and
    never reads this field to do so.
    """

    request_digest: NonEmptyStr
    deciding_principal_id: NonEmptyStr
    decision: DecisionKind
    decided_at: _dt.datetime
    transport: str | None = None

    @model_validator(mode="after")
    def _validate_time(self) -> Self:
        _require_utc(self.decided_at, "decided_at")
        return self


class AdapterCapability(_ApprovalModel):
    """What an adapter actually proves. Both default False: absence is not proof."""

    adapter_id: NonEmptyStr
    adapter_version: NonEmptyStr
    remote_idempotency: bool = False
    reconciliation: bool = False


class EffectAttempt(_ApprovalModel):
    """One durable local attempt to cross the adapter boundary."""

    effect_id: NonEmptyStr
    work_order_id: NonEmptyStr
    attempt_id: NonEmptyStr
    request_digest: NonEmptyStr
    idempotency_key: NonEmptyStr
    action_digest: NonEmptyStr
    destination: NonEmptyStr
    effect_class: NonEmptyStr
    adapter_id: NonEmptyStr
    adapter_version: NonEmptyStr
    state: EffectState
    external_ref: str | None = None
    reconciliation_ref: str | None = None

    @property
    def binding(self) -> dict[str, str]:
        """The fields an idempotency key is bound to; a mismatch is a conflict."""
        return {
            "action_digest": self.action_digest,
            "destination": self.destination,
            "effect_class": self.effect_class,
            "work_order_id": self.work_order_id,
            "attempt_id": self.attempt_id,
        }


class PrepareResult(_ApprovalModel):
    """The outcome of one atomic prepare-or-read.

    ``may_dispatch`` is granted to at most one caller per idempotency key. Every
    other caller sees the same ``attempt`` with ``may_dispatch=False``.
    """

    attempt: EffectAttempt
    may_dispatch: bool


_EVIDENCE_BY_STATE: dict[EffectState, EvidenceResult] = {
    EffectState.ACKNOWLEDGED: EvidenceResult.PASSED,
    EffectState.FAILED: EvidenceResult.FAILED,
    EffectState.BLOCKED: EvidenceResult.BLOCKED,
    EffectState.PREPARED: EvidenceResult.UNKNOWN,
    EffectState.EXECUTING: EvidenceResult.UNKNOWN,
    EffectState.UNKNOWN: EvidenceResult.UNKNOWN,
}


def evidence_projection(attempt: EffectAttempt) -> EvidenceRef:
    """Project an attempt as delivery-consumable evidence.

    ``passed`` here means the destination acknowledged this one effect. It is not
    an aggregate delivery verdict, which only the delivery evaluator may declare.
    """
    return EvidenceRef(
        id=f"evidence:{attempt.idempotency_key}",
        work_order_id=attempt.work_order_id,
        attempt_id=attempt.attempt_id,
        subject_kind=EvidenceSubjectKind.EFFECT,
        subject_id=attempt.effect_id,
        result=_EVIDENCE_BY_STATE[attempt.state],
    )


def effect_ref_projection(attempt: EffectAttempt) -> EffectRef:
    """Project an attempt back onto the PR 704 effect reference."""
    return EffectRef(
        id=attempt.effect_id,
        work_order_id=attempt.work_order_id,
        attempt_id=attempt.attempt_id,
        effect_class=attempt.effect_class,
        destination=attempt.destination,
        idempotency_key=attempt.idempotency_key,
        approval_id=attempt.request_digest,
    )
