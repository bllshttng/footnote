"""Atomic shared-account capacity admission (x-1afa).

One owner answers "may this dispatch consume capacity from this account
budget now?" by reserving a share of the account's subscription windows
BEFORE the launch, under the runtime state's existing file lock, so two
concurrent dispatches cannot both spend the same remaining allowance.

Percentages are the only unit here: remaining window percent minus the
demand already reserved for other dispatches minus this request's demand
must stay at or above the configured reserve, on EVERY binding window of
the account (windows are conjunctive, never averaged). A reservation is an
admission estimate, not a provider-enforced cap: the harness still owns the
session's actual spend, and max_inflight bounds what the estimate can
concurrently claim.

Stale or absent evidence never creates fresh capacity: a stale observation
answers ``stale_observation`` with no reservation persisted, and the caller
falls back to the lane policy it already had (the ``agents.profiles``
on_low/on_unknown behavior this feature deliberately does not duplicate).
"""
from __future__ import annotations

import dataclasses
import time
import uuid
from pathlib import Path
from typing import Any, Optional

from fno.adapters.providers import runtime_state as rs
from fno.config._routing_admission import resolve_admission_policy

#: Positive decision: capacity reserved (or already held for this dispatch).
ADMITTED = "admitted"
#: Refused: some binding window cannot cover demand minus outstanding.
RESERVED_CAPACITY = "reserved_capacity"
#: Refused-soft: evidence stale/absent/partial; the lane policy decides.
STALE_OBSERVATION = "stale_observation"
#: Refused: the budget identity could not be proven for this record.
UNKNOWN_IDENTITY = "unknown_identity"
#: Refused: a binding window is at or past 100 percent, or a lock newer than
#: the probe says the account died. A priority exception never bypasses this.
EXHAUSTED = "exhausted"
#: Refused: the pool already holds max_inflight_per_pool live reservations.
INFLIGHT_CAP = "inflight_cap"
#: Refused: routing.admission is armed but its table failed validation.
INVALID_POLICY = "invalid_policy"

#: The only unit this owner speaks, stamped on every receipt.
UNITS = "subscription-percent"

_TERMINAL_STATES = ("reserved", "committed")


@dataclasses.dataclass(frozen=True)
class AdmissionReceipt:
    """What one admission decision said, with its units and evidence age."""

    status: str
    pool: Optional[str] = None
    reservation_id: Optional[str] = None
    #: The window whose numbers decided, as ``provider/label``.
    binding_window: Optional[str] = None
    #: Percent still ADMITTABLE on the binding window after this decision:
    #: remaining minus outstanding minus demand minus the floor (the
    #: protected reserve, or zero for a priority exception). None when not
    #: computed.
    remaining_admission_pct: Optional[float] = None
    retry_at: Optional[float] = None
    reason: Optional[str] = None
    evidence_age_s: Optional[float] = None
    units: str = UNITS

    @property
    def admitted(self) -> bool:
        return self.status == ADMITTED


def resolve_pool(
    record: Any,
    *,
    by_id: Optional[dict[str, Any]] = None,
    now: float | None = None,
) -> tuple[Optional[str], Optional[str]]:
    """The budget identity a record launches under, with its proof status.

    An operator-declared ``quota_pool`` wins: credentials known to share one
    provider budget are grouped by that key alone. Canonical managed Claude
    uses its observed principal (x-d6be), never the alias or the directory
    basename. Undeclared and unprovable cases keep records separate; an
    unprovable claude oauth principal is a refusal, not an empty pool,
    because unknown usage cannot imply 100 percent available.
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


def _snapshot_for(
    provider_id: str, *, ttl_seconds: float, now: float, repo_root: Path | None
) -> tuple[Any, str]:
    """The usage snapshot and its freshness word (fresh/stale/absent)."""
    raw = None
    try:
        raw = rs._read_disk_payload(rs._resolve_state_path(repo_root))
    except Exception:  # noqa: BLE001 - a corrupt read reads as absent
        raw = None
    usage = rs._parse_usage_payload(raw) if raw else {}
    snap = usage.get(provider_id)
    if snap is None:
        return None, "absent"
    if snap.probed_at < now - ttl_seconds:
        return None, "stale"
    return snap, "fresh"


def decide(
    *,
    snap: Any,
    freshness: str,
    outstanding_pct: float,
    inflight_count: int,
    demand: float,
    reserve: float,
    max_inflight: int,
    now: float,
    consume_reserve: bool = False,
) -> AdmissionReceipt:
    """The pure verdict. Preview and reserve call this same function, so a
    preview never disagrees with the reservation it described."""
    if snap is None or not snap.windows or snap.partial:
        where = freshness if snap is None else ("partial" if snap.partial else "empty")
        return AdmissionReceipt(
            STALE_OBSERVATION,
            reason=f"no whole fresh window observation ({where})",
        )
    binding = [
        w for w in snap.windows if w.resets_at is None or w.resets_at > now
    ]
    observed_age = now - snap.probed_at
    exhausted = [w for w in binding if w.used_pct >= 100.0]
    if exhausted:
        resets = [w.resets_at for w in exhausted if w.resets_at is not None]
        worst = max(exhausted, key=lambda w: w.used_pct)
        return AdmissionReceipt(
            EXHAUSTED,
            binding_window=f"{snap.provider_id}/{worst.label}",
            retry_at=min(resets) if resets else None,
            evidence_age_s=observed_age,
            reason="a binding window is at or past 100 percent",
        )
    if inflight_count >= max_inflight:
        return AdmissionReceipt(
            INFLIGHT_CAP,
            evidence_age_s=observed_age,
            reason=f"pool holds {inflight_count} live reservations >= {max_inflight}",
        )
    # Conjunctive: EVERY binding window must cover the demand. The floor is
    # the protected reserve; a priority exception consumes the reserve but
    # still cannot spend below zero.
    floor = 0.0 if consume_reserve else reserve
    worst_headroom: float | None = None
    worst_window: str | None = None
    for w in binding:
        headroom = 100.0 - w.used_pct - outstanding_pct - demand
        if worst_headroom is None or headroom < worst_headroom:
            worst_headroom, worst_window = headroom, f"{snap.provider_id}/{w.label}"
        if headroom < floor:
            return AdmissionReceipt(
                RESERVED_CAPACITY,
                binding_window=f"{snap.provider_id}/{w.label}",
                remaining_admission_pct=round(headroom - floor, 4),
                evidence_age_s=observed_age,
                reason=(
                    f"window {w.label} at {w.used_pct:.0f}% leaves "
                    f"{headroom:.1f}% after {outstanding_pct:.0f}% reserved and "
                    f"{demand:.0f}% demanded, below the {reserve:.0f}% reserve"
                ),
            )
    return AdmissionReceipt(
        ADMITTED,
        binding_window=worst_window,
        remaining_admission_pct=(
            round(worst_headroom - floor, 4) if worst_headroom is not None else None
        ),
        evidence_age_s=observed_age,
    )


def _outstanding_and_count(
    reservations: dict[str, dict[str, Any]], pool: str
) -> tuple[float, int]:
    """Reserved-demand percent and live-reservation count for one pool."""
    total, count = 0.0, 0
    for record in reservations.values():
        if record.get("pool") != pool or record.get("state") not in _TERMINAL_STATES:
            continue
        count += 1
        total += float(record.get("demand_pct") or 0.0)
    return total, count


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
    """The pure read half. Never writes, never reserves, never expires
    anything: the same decide() the reserve path runs, over a read-only
    view."""
    return _admit(
        record,
        dispatch_id=None,
        verb=verb,
        difficulty=difficulty,
        demand_pct=demand_pct,
        by_id=by_id,
        ttl_seconds=ttl_seconds,
        consume_reserve=consume_reserve,
        policy=policy,
        now=now,
        repo_root=repo_root,
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
    """Preview + persist under the one lock. Idempotent per dispatch id: a
    re-request by the same dispatch returns its held reservation. Two
    different dispatches never share a token."""
    if not dispatch_id or not dispatch_id.strip():
        raise ValueError("reserve_admission: dispatch_id must be non-empty")
    return _admit(
        record,
        dispatch_id=dispatch_id,
        verb=verb,
        difficulty=difficulty,
        demand_pct=demand_pct,
        by_id=by_id,
        ttl_seconds=ttl_seconds,
        consume_reserve=consume_reserve,
        policy=policy,
        now=now,
        repo_root=repo_root,
    )


def _admit(
    record: Any,
    *,
    dispatch_id: str | None,
    verb: str,
    difficulty: str,
    demand_pct: float | None,
    by_id: Optional[dict[str, Any]],
    ttl_seconds: float | None,
    consume_reserve: bool,
    policy: Any | None,
    now: float | None,
    repo_root: Path | None,
) -> AdmissionReceipt:
    if now is None:
        now = time.time()
    if policy is None:
        policy = resolve_admission_policy()
    if not policy.enabled:
        return AdmissionReceipt(
            STALE_OBSERVATION,
            reason="admission is not enabled (config.routing.admission.enabled)",
        )
    if policy.config_errors:
        exact = "; ".join(policy.config_errors.values())
        return AdmissionReceipt(INVALID_POLICY, reason=exact)
    if ttl_seconds is None:
        from fno.adapters.providers.loader import load_quota_config

        ttl_seconds = float(load_quota_config(repo_root=repo_root).probe_ttl_seconds)

    pool, identity_error = resolve_pool(record, by_id=by_id, now=now)
    if pool is None:
        return AdmissionReceipt(UNKNOWN_IDENTITY, reason=identity_error)
    demand = (
        float(demand_pct)
        if demand_pct is not None
        else policy.demand_for(verb, difficulty)
    )
    reserve = policy.reserve_for(verb, difficulty)

    state_path = rs._resolve_state_path(repo_root)

    if dispatch_id is not None:
        # Idempotency and the persist share one lock read: the existing-token
        # check is answered from the same view the write would land on.
        with _lock(state_path):
            raw = rs._read_disk_payload(state_path) or {}
            reservations = rs._drop_expired_reservations(
                rs._parse_reservations_payload(raw), now
            )
            for existing in reservations.values():
                if (
                    existing.get("dispatch_id") == dispatch_id
                    and existing.get("provider_id") == record.id
                    and existing.get("state") in _TERMINAL_STATES
                ):
                    return AdmissionReceipt(
                        ADMITTED,
                        pool=existing.get("pool"),
                        reservation_id=existing.get("reservation_id"),
                        reason="existing reservation for this dispatch",
                    )
            outstanding, count = _outstanding_and_count(reservations, pool)
            snap, freshness = _snapshot_for(
                record.id, ttl_seconds=ttl_seconds, now=now, repo_root=repo_root
            )
            verdict = decide(
                snap=snap,
                freshness=freshness,
                outstanding_pct=outstanding,
                inflight_count=count,
                demand=demand,
                reserve=reserve,
                max_inflight=policy.max_inflight_per_pool,
                now=now,
                consume_reserve=consume_reserve,
            )
            if not verdict.admitted:
                return verdict
            rid = f"adm-{uuid.uuid4().hex[:8]}"
            record_out: dict[str, Any] = {
                "reservation_id": rid,
                "pool": pool,
                "provider_id": record.id,
                "dispatch_id": dispatch_id,
                "session_id": None,
                "verb": verb,
                "difficulty": difficulty,
                "demand_pct": demand,
                "state": "reserved",
                "observed_at": getattr(snap, "probed_at", None),
                "binding_windows": [
                    {
                        "provider_id": snap.provider_id,
                        "label": w.label,
                        "used_pct": w.used_pct,
                        "resets_at": w.resets_at,
                    }
                    for w in snap.windows
                ]
                if snap is not None
                else [],
                "created_at": now,
                "expires_at": now + policy.reservation_ttl_seconds,
            }
            reservations[rid] = record_out
            _persist(state_path, raw, reservations)
            return AdmissionReceipt(
                ADMITTED,
                pool=pool,
                reservation_id=rid,
                binding_window=verdict.binding_window,
                remaining_admission_pct=verdict.remaining_admission_pct,
                evidence_age_s=verdict.evidence_age_s,
            )
    # Preview path: no lock, no write, no idempotency.
    raw_view: dict[str, Any] | None = None
    try:
        raw_view = rs._read_disk_payload(state_path)
    except Exception:  # noqa: BLE001 - a corrupt read reads as empty
        raw_view = None
    reservations = rs._drop_expired_reservations(
        rs._parse_reservations_payload(raw_view or {}), now
    )
    outstanding, count = _outstanding_and_count(reservations, pool)
    snap, freshness = _snapshot_for(
        record.id, ttl_seconds=ttl_seconds, now=now, repo_root=repo_root
    )
    return decide(
        snap=snap,
        freshness=freshness,
        outstanding_pct=outstanding,
        inflight_count=count,
        demand=demand,
        reserve=reserve,
        max_inflight=policy.max_inflight_per_pool,
        now=now,
        consume_reserve=consume_reserve,
    )


def _lock(state_path: Path):
    import filelock

    return filelock.FileLock(str(rs._lock_path(state_path)), timeout=rs.LOCK_TIMEOUT_SECONDS)


def _persist(
    state_path: Path,
    raw: dict[str, Any],
    reservations: dict[str, dict[str, Any]],
) -> None:
    """Rewrite the state document with new reservations, every other block
    carried verbatim (the writer pattern every other mutation here uses)."""
    rs._write_state_atomic(
        state_path,
        rs._serialize_state(
            rs.ProviderRuntimeState(
                provider_health=rs._parse_state_payload(raw),
                combo_cursors=rs._parse_cursors_payload(raw),
                usage=rs._parse_usage_payload(raw),
                windows_opened=rs._parse_windows_opened(raw),
                reservations=reservations,
                schema_version=int(raw.get("schema_version", rs.SCHEMA_VERSION)),
            )
        ),
    )


def commit_reservation(
    reservation_id: str,
    *,
    dispatch_id: str,
    session_id: str | None = None,
    ttl_seconds: int | None = None,
    policy: Any | None = None,
    now: float | None = None,
    repo_root: Path | None = None,
) -> bool:
    """Stamp a real session onto the reservation and extend its lease. A
    committed reservation is the receipt that the launch happened."""
    return _mutate(
        reservation_id,
        dispatch_id=dispatch_id,
        ttl_seconds=ttl_seconds,
        policy=policy,
        now=now,
        repo_root=repo_root,
        mutator=lambda record: record.update(
            {"state": "committed", "session_id": session_id}
        ),
    )


def refresh_reservation(
    reservation_id: str,
    *,
    dispatch_id: str,
    ttl_seconds: int | None = None,
    policy: Any | None = None,
    now: float | None = None,
    repo_root: Path | None = None,
) -> bool:
    """Extend the lease while reconciliation keeps proving the worker live.
    A live worker past its TTL is revalidated, never refunded."""
    return _mutate(
        reservation_id,
        dispatch_id=dispatch_id,
        ttl_seconds=ttl_seconds,
        policy=policy,
        now=now,
        repo_root=repo_root,
        mutator=lambda record: None,
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
        reservation_id,
        dispatch_id=dispatch_id,
        ttl_seconds=None,
        policy=policy,
        now=now,
        repo_root=repo_root,
        mutator=None,
    )


def _mutate(
    reservation_id: str,
    *,
    dispatch_id: str,
    ttl_seconds: int | None,
    policy: Any | None,
    now: float | None,
    repo_root: Path | None,
    mutator,  # Callable[[dict], None] | None; None = remove
) -> bool:
    if now is None:
        now = time.time()
    if ttl_seconds is None:
        # Same basis as the reserve: an injected policy wins, the live
        # config answers otherwise, so the lease never changes owners'
        # minds about its own expiry.
        if policy is not None:
            ttl_seconds = policy.reservation_ttl_seconds
        else:
            ttl_seconds = resolve_admission_policy().reservation_ttl_seconds
    state_path = rs._resolve_state_path(repo_root)
    with _lock(state_path):
        raw = rs._read_disk_payload(state_path)
        if raw is None:
            return False
        reservations = rs._drop_expired_reservations(
            rs._parse_reservations_payload(raw), now
        )
        record = reservations.get(reservation_id)
        if record is None or record.get("dispatch_id") != dispatch_id:
            return False
        if mutator is None:
            del reservations[reservation_id]
        else:
            mutator(record)
            record["expires_at"] = now + ttl_seconds
        _persist(state_path, raw, reservations)
        return True


def outstanding_for_pool(
    pool: str,
    *,
    now: float | None = None,
    repo_root: Path | None = None,
) -> tuple[float, int]:
    """Read-only: reserved demand percent and live-reservation count."""
    if now is None:
        now = time.time()
    try:
        state = rs.read_state(now=now)
    except Exception:  # noqa: BLE001 - an unreadable state reads as idle
        return 0.0, 0
    return _outstanding_and_count(state.reservations, pool)


def reservations_snapshot(
    *,
    now: float | None = None,
    repo_root: Path | None = None,
) -> dict[str, dict[str, Any]]:
    """Read-only view for inventory/explain. Preview callers render from a
    copy; byte-identical repeat renders are their job, not this reader's."""
    if now is None:
        now = time.time()
    try:
        state = rs.read_state(now=now)
    except Exception:  # noqa: BLE001 - an unreadable state reads as idle
        return {}
    return {
        rid: dict(record)
        for rid, record in rs._drop_expired_reservations(
            state.reservations, now
        ).items()
    }
