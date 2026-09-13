"""Shared-account capacity admission: the Python side of the seam.

Python resolves everything that needs the operator's machine - the budget
identity (the proven principal, a Keychain read), the policy from config,
the state path, the probe TTL - and sends the ``fno-agents admission`` verb
one JSON payload. The math and the disk live in Rust
(``crates/fno-agents/src/admission.rs``); this module is the transport plus
the identity resolver, never a second decider.
"""
from __future__ import annotations

import dataclasses
import time
from pathlib import Path
from typing import Any, Optional

from fno.adapters.providers import runtime_state as rs
from fno.config._routing_admission import resolve_admission_policy
from fno.rust_binary import VerbUnavailable, verb_call

#: Positive decision: capacity reserved (or already held for this dispatch).
ADMITTED = "admitted"
#: Refused: some binding window cannot cover demand minus outstanding.
RESERVED_CAPACITY = "reserved_capacity"
#: Refused-soft: the evidence is stale, absent, or partial; the lane policy
#: decides what happens next. Never a persisted reservation.
STALE_OBSERVATION = "stale_observation"
#: Refused: the budget identity could not be proven for this record.
UNKNOWN_IDENTITY = "unknown_identity"
#: Refused: a binding window is at or past 100 percent. A priority exception
#: never bypasses this.
EXHAUSTED = "exhausted"
#: Refused: the pool already holds max_inflight_per_pool live reservations.
INFLIGHT_CAP = "inflight_cap"
#: Refused: routing.admission is armed but its table failed validation.
INVALID_POLICY = "invalid_policy"

#: The only unit this owner speaks, stamped on every receipt.
UNITS = "subscription-percent"

#: Refusal statuses that hold a launch even for a priority exception.
REFUSAL_STATUSES = frozenset(
    {RESERVED_CAPACITY, EXHAUSTED, INFLIGHT_CAP, UNKNOWN_IDENTITY, INVALID_POLICY}
)


@dataclasses.dataclass(frozen=True)
class AdmissionReceipt:
    """What one admission decision said, with its units and evidence age."""

    status: str
    pool: Optional[str] = None
    reservation_id: Optional[str] = None
    #: The window whose numbers decided, as ``provider/label``.
    binding_window: Optional[str] = None
    #: Percent still ADMITTABLE on the binding window after this decision.
    remaining_admission_pct: Optional[float] = None
    retry_at: Optional[float] = None
    reason: Optional[str] = None
    evidence_age_s: Optional[float] = None
    #: The priced axes, so a display never re-derives the lookup.
    demand_applied: Optional[float] = None
    reserve_applied: Optional[float] = None
    units: str = UNITS

    @property
    def admitted(self) -> bool:
        return self.status == ADMITTED

    @classmethod
    def from_json(cls, raw: dict[str, Any]) -> "AdmissionReceipt":
        return cls(
            status=str(raw.get("status") or ""),
            pool=raw.get("pool"),
            reservation_id=raw.get("reservation_id"),
            binding_window=raw.get("binding_window"),
            remaining_admission_pct=raw.get("remaining_admission_pct"),
            retry_at=raw.get("retry_at"),
            reason=raw.get("reason"),
            evidence_age_s=raw.get("evidence_age_s"),
            demand_applied=raw.get("demand_applied"),
            reserve_applied=raw.get("reserve_applied"),
            units=str(raw.get("units") or UNITS),
        )


class AdmissionUnavailable(VerbUnavailable):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def resolve_pool(
    record: Any,
    *,
    by_id: Optional[dict[str, Any]] = None,
    now: float | None = None,
) -> tuple[Optional[str], Optional[str]]:
    """The budget identity a record launches under, with its proof status.

    An operator-declared ``quota_pool`` wins: credentials known to share one
    provider budget are grouped by that key alone. Canonical managed Claude
    uses its observed principal, never the alias or the directory basename.
    Undeclared and unprovable cases keep records separate; an unprovable
    claude oauth principal is a refusal, not an empty pool, because unknown
    usage cannot imply 100 percent available. This half stays in Python on
    purpose: the proof reads the operator's Keychain.
    """
    declared = getattr(record, "quota_pool", None)
    if declared:
        return f"declared:{declared}", None
    if getattr(record, "auth", "") == "api_key":
        return f"api:{record.harness}/{record.id}", None
    if record.harness == "claude":
        from fno.adapters.providers.binding import MATCHED, resolve_account_binding

        binding = resolve_account_binding(record, by_id=by_id, now=now)
        if binding.status == MATCHED and binding.observed_principal:
            return f"principal:{binding.observed_principal}", None
        return None, binding.receipt
    # Non-claude oauth has no principal probe; records stay separate.
    return f"{record.harness}/{record.id}", None


def _policy_payload(policy: Any) -> dict[str, Any]:
    return {
        "enabled": policy.enabled,
        "max_inflight_per_pool": policy.max_inflight_per_pool,
        "reservation_ttl_seconds": policy.reservation_ttl_seconds,
        "demand_pct": policy.demand_pct,
        "reserve_pct": policy.reserve_pct,
        "config_errors": policy.config_errors or {},
        "config_error_text": "; ".join(policy.config_errors.values())
        if policy.config_errors
        else None,
    }


def _payload(
    mode: str,
    *,
    record: Any,
    pool: Optional[str],
    identity_error: Optional[str],
    policy: Any,
    dispatch_id: str | None = None,
    reservation_id: str | None = None,
    session_id: str | None = None,
    verb: str = "do",
    difficulty: str = "high",
    demand_pct: float | None = None,
    consume_reserve: bool = False,
    ttl_seconds: float | None = None,
    now: float | None = None,
    state_path: Path | None = None,
    state: dict[str, Any] | None = None,
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "mode": mode,
        "policy": _policy_payload(policy),
        "record": {"id": record.id},
        "pool": pool,
        "identity_error": identity_error,
        "verb": verb,
        "difficulty": difficulty,
        "consume_reserve": consume_reserve,
        "ttl_seconds": ttl_seconds,
        "now": now,
    }
    if demand_pct is not None:
        payload["demand_pct"] = float(demand_pct)
    if dispatch_id is not None:
        payload["dispatch_id"] = dispatch_id
    if reservation_id is not None:
        payload["reservation_id"] = reservation_id
    if session_id is not None:
        payload["session_id"] = session_id
    if state_path is not None:
        payload["state_path"] = str(state_path)
    if state is not None:
        payload["state"] = state
    return payload


def _call(payload: dict[str, Any]) -> dict[str, Any]:
    return verb_call("admission", payload, AdmissionUnavailable)


def _enabled_policy(policy: Any | None) -> Any:
    if policy is None:
        policy = resolve_admission_policy()
    return policy


def _read_state(repo_root: Path | None) -> dict[str, Any]:
    try:
        return rs._read_disk_payload(rs._resolve_state_path(repo_root)) or {}
    except Exception:  # noqa: BLE001 - a corrupt read reads as empty
        return {}


def preview_admission(
    record: Any,
    *,
    verb: str = "do",
    difficulty: str = "high",
    demand_pct: float | None = None,
    by_id: Optional[dict[str, Any]] = None,
    ttl_seconds: float | None = None,
    consume_reserve: bool = False,
    policy: Any | None = None,
    now: float | None = None,
    repo_root: Path | None = None,
) -> AdmissionReceipt:
    """The pure read half. Never writes, never reserves: the same decide the
    reserve path runs, over a state document read without the lock."""
    if now is None:
        now = time.time()
    policy = _enabled_policy(policy)
    pool, identity_error = resolve_pool(record, by_id=by_id, now=now)
    payload = _payload(
        "preview",
        record=record,
        pool=pool,
        identity_error=identity_error,
        policy=policy,
        verb=verb,
        difficulty=difficulty,
        demand_pct=demand_pct,
        consume_reserve=consume_reserve,
        ttl_seconds=ttl_seconds,
        now=now,
        state=_read_state(repo_root),
    )
    try:
        return AdmissionReceipt.from_json(_call(payload))
    except AdmissionUnavailable as exc:
        # A missing Rust leg degrades open WITHOUT reserving: the caller
        # keeps whatever verdict the lane policy already had.
        return AdmissionReceipt(
            STALE_OBSERVATION,
            reason=f"admission unavailable: {exc}",
        )


def reserve_admission(
    record: Any,
    *,
    dispatch_id: str,
    verb: str = "do",
    difficulty: str = "high",
    demand_pct: float | None = None,
    by_id: Optional[dict[str, Any]] = None,
    ttl_seconds: float | None = None,
    consume_reserve: bool = False,
    policy: Any | None = None,
    now: float | None = None,
    repo_root: Path | None = None,
) -> AdmissionReceipt:
    """Preview + persist under the one lock. Idempotent per dispatch id and
    record: a re-request by the same dispatch returns its held reservation.
    Two different dispatches never share a token."""
    if not dispatch_id or not dispatch_id.strip():
        raise ValueError("reserve_admission: dispatch_id must be non-empty")
    if now is None:
        now = time.time()
    policy = _enabled_policy(policy)
    pool, identity_error = resolve_pool(record, by_id=by_id, now=now)
    payload = _payload(
        "reserve",
        record=record,
        pool=pool,
        identity_error=identity_error,
        policy=policy,
        dispatch_id=dispatch_id,
        verb=verb,
        difficulty=difficulty,
        demand_pct=demand_pct,
        consume_reserve=consume_reserve,
        ttl_seconds=ttl_seconds,
        now=now,
        state_path=rs._resolve_state_path(repo_root),
    )
    return AdmissionReceipt.from_json(_call(payload))


def _mutate(
    mode: str,
    reservation_id: str,
    *,
    dispatch_id: str,
    session_id: str | None = None,
    ttl_seconds: float | None = None,
    policy: Any | None = None,
    now: float | None = None,
    repo_root: Path | None = None,
) -> bool:
    policy = _enabled_policy(policy)
    payload = _payload(
        mode,
        record=_Recordless(),
        pool=None,
        identity_error=None,
        policy=policy,
        dispatch_id=dispatch_id,
        reservation_id=reservation_id,
        session_id=session_id,
        ttl_seconds=ttl_seconds,
        now=now,
        state_path=rs._resolve_state_path(repo_root),
    )
    try:
        answer = _call(payload)
    except AdmissionUnavailable:
        return False
    return bool(answer.get("ok"))


class _Recordless:
    """A payload shim for the mutate modes: the Rust side reads only the
    reservation row for these modes and never touches record.id."""

    id = ""


def commit_reservation(
    reservation_id: str,
    *,
    dispatch_id: str,
    session_id: str | None = None,
    ttl_seconds: float | None = None,
    policy: Any | None = None,
    now: float | None = None,
    repo_root: Path | None = None,
) -> bool:
    """Stamp a real session onto the reservation and extend its lease. A
    committed reservation is the receipt that the launch happened."""
    return _mutate(
        "commit",
        reservation_id,
        dispatch_id=dispatch_id,
        session_id=session_id,
        ttl_seconds=ttl_seconds,
        policy=policy,
        now=now,
        repo_root=repo_root,
    )


def refresh_reservation(
    reservation_id: str,
    *,
    dispatch_id: str,
    ttl_seconds: float | None = None,
    policy: Any | None = None,
    now: float | None = None,
    repo_root: Path | None = None,
) -> bool:
    """Extend the lease while reconciliation keeps proving the worker live.
    A live worker past its TTL is revalidated, never refunded."""
    return _mutate(
        "refresh",
        reservation_id,
        dispatch_id=dispatch_id,
        ttl_seconds=ttl_seconds,
        policy=policy,
        now=now,
        repo_root=repo_root,
    )


def release_reservation(
    reservation_id: str,
    *,
    dispatch_id: str,
    policy: Any | None = None,
    now: float | None = None,
    repo_root: Path | None = None,
) -> bool:
    """Remove one failed launch's reservation. Only the dispatch that holds
    the token can release it, so one worker's cleanup never refunds another."""
    return _mutate(
        "release",
        reservation_id,
        dispatch_id=dispatch_id,
        policy=policy,
        now=now,
        repo_root=repo_root,
    )


def outstanding_for_pool(
    pool: str,
    *,
    now: float | None = None,
    repo_root: Path | None = None,
) -> tuple[float, int]:
    """Read-only: reserved demand percent and live-reservation count. A local
    read through the state parser; never a subprocess round-trip."""
    if now is None:
        now = time.time()
    try:
        state = rs.read_state(now=now)
    except Exception:  # noqa: BLE001 - an unreadable state reads as idle
        return 0.0, 0
    total, count = 0.0, 0
    for rec in state.reservations.values():
        if rec.get("pool") != pool or rec.get("state") not in ("reserved", "committed"):
            continue
        count += 1
        total += float(rec.get("demand_pct") or 0.0)
    return total, count


def reservations_snapshot(
    *,
    now: float | None = None,
    repo_root: Path | None = None,
) -> dict[str, dict[str, Any]]:
    """Read-only view for inventory/explain. Byte-identical repeat renders
    are the preview caller's job, not this reader's."""
    if now is None:
        now = time.time()
    try:
        state = rs.read_state(now=now)
    except Exception:  # noqa: BLE001 - an unreadable state reads as idle
        return {}
    now_ms = now
    return {
        rid: dict(record)
        for rid, record in state.reservations.items()
        if float(record.get("expires_at") or 0.0) > now_ms
    }
