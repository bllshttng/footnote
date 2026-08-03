"""Acceptance tests for the approval store and effect journal (x-4530)."""

from __future__ import annotations

import datetime as _dt
import json
from pathlib import Path

import pytest

from fno.approvals import (
    AdapterCapability,
    ApprovalRequest,
    DecisionKind,
    EffectState,
    EffectStore,
    RefusalReason,
    RefusedError,
    ReconciliationRead,
    action_digest,
    classify_effect,
    effect_ref_projection,
    evidence_projection,
)
from fno.approvals.models import EffectDisposition
from fno.company.contracts import EvidenceResult, EvidenceSubjectKind

FOUNDER = "principal:founder"
INTERN = "principal:intern"
NOW = _dt.datetime(2026, 8, 2, 12, 0, tzinfo=_dt.timezone.utc)


class StubAuthority:
    """Stands in for independent company policy. Never reads a decision payload."""

    source = "test-policy"

    def __init__(self, allowed: set[str] | None = None) -> None:
        self.allowed = {FOUNDER} if allowed is None else allowed
        self.calls: list[dict[str, str]] = []

    def may_approve(self, *, principal_id: str, effect_class: str, destination: str) -> bool:
        self.calls.append(
            {
                "principal_id": principal_id,
                "effect_class": effect_class,
                "destination": destination,
            }
        )
        return principal_id in self.allowed


def _request(**overrides: object) -> ApprovalRequest:
    fields: dict[str, object] = {
        "request_id": "req-1",
        "principal_id": FOUNDER,
        "work_order_id": "x-1111",
        "attempt_id": "attempt-1",
        "effect_id": "effect-1",
        "effect_class": "communication.external",
        "destination": "email:customer@example.com",
        "action_digest": action_digest({"subject": "hello", "body": "original"}),
        "created_at": NOW,
        "expires_at": NOW + _dt.timedelta(hours=1),
    }
    fields.update(overrides)
    return ApprovalRequest(**fields)  # type: ignore[arg-type]


ADAPTER = AdapterCapability(adapter_id="smtp", adapter_version="1")
BLIND_ADAPTER = AdapterCapability(adapter_id="webhook", adapter_version="1")
IDEMPOTENT_ADAPTER = AdapterCapability(
    adapter_id="stripe-like", adapter_version="1", remote_idempotency=True
)
RECONCILING_ADAPTER = AdapterCapability(
    adapter_id="ticketing", adapter_version="1", reconciliation=True
)


@pytest.fixture
def clock() -> list[_dt.datetime]:
    return [NOW]


@pytest.fixture
def store(tmp_path: Path, clock: list[_dt.datetime]) -> EffectStore:
    authority = StubAuthority()
    with EffectStore(
        tmp_path / "approvals.db",
        authority=authority,
        events_path=tmp_path / "events.jsonl",
        now=lambda: clock[0],
    ) as opened:
        opened.authority = authority  # type: ignore[attr-defined]
        yield opened


def _events(tmp_path: Path) -> list[dict]:
    path = tmp_path / "events.jsonl"
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def _approve(store: EffectStore, request: ApprovalRequest) -> str:
    store.submit(request)
    store.decide(
        request_digest=request.request_digest,
        deciding_principal_id=FOUNDER,
        decision=DecisionKind.APPROVED,
    )
    return request.request_digest


# ── AC-A1-SEC: approval binds the exact effect ──────────────────────────────


def test_ac1_sec_exact_match_is_permitted(store: EffectStore) -> None:
    digest = _approve(store, _request())
    result = store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    assert result.may_dispatch is True
    assert result.attempt.state is EffectState.PREPARED


@pytest.mark.parametrize(
    "field,value",
    [
        ("action_digest", action_digest({"subject": "hello", "body": "EDITED"})),
        ("destination", "email:someone-else@example.com"),
        ("effect_class", "publication.public"),
        ("attempt_id", "attempt-2"),
        ("principal_id", INTERN),
        ("expires_at", NOW + _dt.timedelta(hours=2)),
    ],
)
def test_ac1_sec_changing_any_bound_field_requires_a_new_approval(
    store: EffectStore, field: str, value: object
) -> None:
    approved = _request()
    _approve(store, approved)
    altered = _request(**{field: value})

    assert altered.request_digest != approved.request_digest
    with pytest.raises(RefusedError) as excinfo:
        store.prepare(
            request_digest=altered.request_digest, idempotency_key="key-1", adapter=ADAPTER
        )
    assert excinfo.value.refusal.reason is RefusalReason.DIGEST_MISMATCH
    assert store.get_attempt("key-1") is None


def test_ac1_sec_expired_decision_never_dispatches(
    store: EffectStore, clock: list[_dt.datetime]
) -> None:
    request = _request()
    digest = _approve(store, request)
    clock[0] = request.expires_at + _dt.timedelta(seconds=1)

    with pytest.raises(RefusedError) as excinfo:
        store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    assert excinfo.value.refusal.reason is RefusalReason.EXPIRED
    assert store.get_attempt("key-1") is None


# ── AC-A2-SEC: transports and roles cannot mint authority ───────────────────


def test_ac2_sec_unauthorized_principal_is_refused(store: EffectStore) -> None:
    request = _request()
    store.submit(request)

    with pytest.raises(RefusedError) as excinfo:
        store.decide(
            request_digest=request.request_digest,
            deciding_principal_id=INTERN,
            decision=DecisionKind.APPROVED,
        )

    refusal = excinfo.value.refusal
    assert refusal.reason is RefusalReason.UNAUTHORIZED_PRINCIPAL
    assert refusal.authority_source == "test-policy"
    assert store.get_decision(request.request_digest) is None


@pytest.mark.parametrize("transport", ["cli", "web", "chat", "plugin:slack", "role:comms"])
def test_ac2_sec_no_transport_widens_the_authorized_set(
    store: EffectStore, transport: str
) -> None:
    request = _request()
    store.submit(request)

    with pytest.raises(RefusedError) as excinfo:
        store.decide(
            request_digest=request.request_digest,
            deciding_principal_id=INTERN,
            decision=DecisionKind.APPROVED,
            transport=transport,
        )
    assert excinfo.value.refusal.reason is RefusalReason.UNAUTHORIZED_PRINCIPAL
    assert store.get_decision(request.request_digest) is None


def test_ac2_sec_authority_is_consulted_with_only_policy_relevant_fields(
    store: EffectStore,
) -> None:
    request = _request()
    store.submit(request)
    store.decide(
        request_digest=request.request_digest,
        deciding_principal_id=FOUNDER,
        decision=DecisionKind.APPROVED,
        transport="chat",
    )
    call = store.authority.calls[-1]  # type: ignore[attr-defined]
    assert call == {
        "principal_id": FOUNDER,
        "effect_class": "communication.external",
        "destination": "email:customer@example.com",
    }


def test_ac2_sec_authority_revoked_after_decision_blocks_execution(store: EffectStore) -> None:
    digest = _approve(store, _request())
    store.authority.allowed = set()  # type: ignore[attr-defined]

    with pytest.raises(RefusedError) as excinfo:
        store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    assert excinfo.value.refusal.reason is RefusalReason.UNAUTHORIZED_PRINCIPAL


# ── AC-A3-CON: concurrent retries have one local dispatcher ─────────────────


def test_ac3_con_concurrent_prepares_share_one_attempt_and_one_dispatcher(
    tmp_path: Path,
) -> None:
    db = tmp_path / "approvals.db"
    request = _request()
    with EffectStore(
        db,
        authority=StubAuthority(),
        events_path=tmp_path / "events.jsonl",
        now=lambda: NOW,
    ) as writer:
        digest = _approve(writer, request)

    # Two independent connections, as two processes would hold.
    stores = [
        EffectStore(
            db,
            authority=StubAuthority(),
            events_path=tmp_path / "events.jsonl",
            now=lambda: NOW,
        )
        for _ in range(2)
    ]
    try:
        results = [
            store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
            for store in stores
        ]
    finally:
        for store in stores:
            store.close()

    assert {r.attempt.idempotency_key for r in results} == {"key-1"}
    assert {r.attempt.request_digest for r in results} == {digest}
    assert sum(1 for r in results if r.may_dispatch) == 1


def test_ac3_con_repeated_prepare_never_regrants_dispatch(store: EffectStore) -> None:
    digest = _approve(store, _request())
    first = store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    assert first.may_dispatch is True

    for _ in range(3):
        again = store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
        assert again.may_dispatch is False
        assert again.attempt.effect_id == first.attempt.effect_id


# ── AC-A4-ERR: ambiguous external outcomes remain unknown ───────────────────


def test_ac4_err_ambiguous_outcome_settles_unknown_and_keeps_the_key(
    store: EffectStore,
) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.EXECUTING)

    attempt = store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)
    assert attempt.state is EffectState.UNKNOWN
    assert attempt.idempotency_key == "key-1"
    assert attempt.external_ref is None


def test_ac4_err_unknown_retry_is_refused_without_a_proven_capability(
    store: EffectStore,
) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)

    with pytest.raises(RefusedError) as excinfo:
        store.authorize_retry(idempotency_key="key-1", adapter=BLIND_ADAPTER)

    refusal = excinfo.value.refusal
    assert refusal.reason is RefusalReason.UNSAFE_RETRY
    assert refusal.recovery is not None and "reconciliation" in refusal.recovery.lower()
    assert store.get_attempt("key-1").state is EffectState.UNKNOWN


def test_ac4_err_remote_idempotency_alone_makes_an_unknown_retry_safe(
    store: EffectStore,
) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)

    reopened = store.authorize_retry(idempotency_key="key-1", adapter=IDEMPOTENT_ADAPTER)
    assert reopened.state is EffectState.EXECUTING
    assert reopened.idempotency_key == "key-1"


def test_ac4_err_a_reconciliation_read_proving_absence_makes_a_retry_safe(
    store: EffectStore,
) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)

    reopened = store.authorize_retry(
        idempotency_key="key-1",
        adapter=RECONCILING_ADAPTER,
        reconciliation=ReconciliationRead(ref="read-1", effect_present=False),
    )
    assert reopened.state is EffectState.EXECUTING
    assert reopened.reconciliation_ref == "read-1"


def test_ac4_err_reconciliation_capability_without_the_read_is_not_proof(
    store: EffectStore,
) -> None:
    """Being able to read the destination is not the same as having read it."""
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)

    with pytest.raises(RefusedError) as excinfo:
        store.authorize_retry(idempotency_key="key-1", adapter=RECONCILING_ADAPTER)

    assert excinfo.value.refusal.reason is RefusalReason.UNSAFE_RETRY
    assert store.get_attempt("key-1").state is EffectState.UNKNOWN


def test_ac4_err_a_read_finding_the_effect_present_refuses_the_retry(
    store: EffectStore,
) -> None:
    """The effect already landed, so resending it would duplicate it."""
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)

    with pytest.raises(RefusedError) as excinfo:
        store.authorize_retry(
            idempotency_key="key-1",
            adapter=RECONCILING_ADAPTER,
            reconciliation=ReconciliationRead(ref="read-1", effect_present=True),
        )

    refusal = excinfo.value.refusal
    assert refusal.reason is RefusalReason.UNSAFE_RETRY
    assert "effect_present" in refusal.fields
    assert refusal.recovery is not None and "acknowledged" in refusal.recovery
    assert store.get_attempt("key-1").state is EffectState.UNKNOWN


def test_ac4_err_a_read_cannot_substitute_for_an_adapter_that_cannot_read(
    store: EffectStore,
) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)

    with pytest.raises(RefusedError) as excinfo:
        store.authorize_retry(
            idempotency_key="key-1",
            adapter=BLIND_ADAPTER,
            reconciliation=ReconciliationRead(ref="read-1", effect_present=False),
        )
    assert excinfo.value.refusal.reason is RefusalReason.UNSAFE_RETRY


@pytest.mark.parametrize("state", [EffectState.PREPARED, EffectState.EXECUTING])
def test_ac3_con_an_in_flight_attempt_cannot_be_reopened(
    store: EffectStore, state: EffectState
) -> None:
    """An in-flight attempt already has a live dispatcher; reopening it would
    hand a second caller dispatch on the same effect."""
    digest = _approve(store, _request())
    first = store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    assert first.may_dispatch is True
    if state is EffectState.EXECUTING:
        store.settle(idempotency_key="key-1", state=EffectState.EXECUTING)

    with pytest.raises(RefusedError) as excinfo:
        store.authorize_retry(idempotency_key="key-1", adapter=IDEMPOTENT_ADAPTER)

    assert excinfo.value.refusal.reason is RefusalReason.UNSAFE_RETRY
    assert store.get_attempt("key-1").state is state


def test_ac3_con_a_reopened_attempt_cannot_be_reopened_again(store: EffectStore) -> None:
    """Two workers retrying one failed attempt: the first reopens it to executing,
    and the second must not be handed dispatch on the same effect."""
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.FAILED)

    assert store.authorize_retry(
        idempotency_key="key-1", adapter=ADAPTER
    ).state is EffectState.EXECUTING

    with pytest.raises(RefusedError) as excinfo:
        store.authorize_retry(idempotency_key="key-1", adapter=ADAPTER)
    assert excinfo.value.refusal.reason is RefusalReason.UNSAFE_RETRY


def test_ac1_sec_retry_cannot_outlive_the_approval(
    store: EffectStore, clock: list[_dt.datetime]
) -> None:
    """prepare() revalidates expiration at execution time; so must a retry."""
    request = _request()
    digest = _approve(store, request)
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.FAILED)
    clock[0] = request.expires_at + _dt.timedelta(seconds=1)

    with pytest.raises(RefusedError) as excinfo:
        store.authorize_retry(idempotency_key="key-1", adapter=IDEMPOTENT_ADAPTER)
    assert excinfo.value.refusal.reason is RefusalReason.EXPIRED
    assert store.get_attempt("key-1").state is EffectState.FAILED


def test_ac2_sec_retry_cannot_outlive_the_principal_authority(store: EffectStore) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.FAILED)
    store.authority.allowed = set()  # type: ignore[attr-defined]

    with pytest.raises(RefusedError) as excinfo:
        store.authorize_retry(idempotency_key="key-1", adapter=IDEMPOTENT_ADAPTER)
    assert excinfo.value.refusal.reason is RefusalReason.UNAUTHORIZED_PRINCIPAL
    assert store.get_attempt("key-1").state is EffectState.FAILED


def test_ac4_err_failed_retry_needs_no_capability_proof(store: EffectStore) -> None:
    """An explicit rejection means the effect did not happen, so a retry cannot
    duplicate it. Only an ambiguous outcome needs the capability check."""
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.FAILED)

    reopened = store.authorize_retry(idempotency_key="key-1", adapter=BLIND_ADAPTER)
    assert reopened.state is EffectState.EXECUTING
    assert reopened.idempotency_key == "key-1"


def test_ac4_err_unknown_cannot_be_silently_settled_as_executing(store: EffectStore) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)

    with pytest.raises(RefusedError):
        store.settle(idempotency_key="key-1", state=EffectState.EXECUTING)


def test_ac4_err_reconciliation_resolves_unknown_honestly(store: EffectStore) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)

    resolved = store.settle(
        idempotency_key="key-1",
        state=EffectState.ACKNOWLEDGED,
        reconciliation_ref="destination-read-1",
    )
    assert resolved.state is EffectState.ACKNOWLEDGED
    assert resolved.reconciliation_ref == "destination-read-1"


# ── AC-A5-ERR: decline and expiration never execute ─────────────────────────


def test_ac5_err_declined_request_is_terminal(store: EffectStore, tmp_path: Path) -> None:
    request = _request()
    store.submit(request)
    store.decide(
        request_digest=request.request_digest,
        deciding_principal_id=FOUNDER,
        decision=DecisionKind.DECLINED,
    )

    with pytest.raises(RefusedError) as excinfo:
        store.prepare(
            request_digest=request.request_digest, idempotency_key="key-1", adapter=ADAPTER
        )
    assert excinfo.value.refusal.reason is RefusalReason.DECLINED
    assert store.get_attempt("key-1") is None

    types = [event["type"] for event in _events(tmp_path)]
    assert "effect_state_changed" not in types


def test_ac5_err_undecided_request_never_executes(store: EffectStore) -> None:
    request = _request()
    store.submit(request)

    with pytest.raises(RefusedError) as excinfo:
        store.prepare(
            request_digest=request.request_digest, idempotency_key="key-1", adapter=ADAPTER
        )
    assert excinfo.value.refusal.reason is RefusalReason.NOT_APPROVED


def test_ac5_err_a_decided_request_is_immutable(store: EffectStore) -> None:
    request = _request()
    store.submit(request)
    store.decide(
        request_digest=request.request_digest,
        deciding_principal_id=FOUNDER,
        decision=DecisionKind.DECLINED,
    )

    with pytest.raises(RefusedError) as excinfo:
        store.decide(
            request_digest=request.request_digest,
            deciding_principal_id=FOUNDER,
            decision=DecisionKind.APPROVED,
        )
    assert excinfo.value.refusal.reason is RefusalReason.REPLAY
    decision = store.get_decision(request.request_digest)
    assert decision is not None and decision.decision is DecisionKind.DECLINED


def test_ac5_err_repeating_the_same_decision_returns_the_existing_one(
    store: EffectStore, tmp_path: Path
) -> None:
    request = _request()
    store.submit(request)
    first = store.decide(
        request_digest=request.request_digest,
        deciding_principal_id=FOUNDER,
        decision=DecisionKind.APPROVED,
    )
    second = store.decide(
        request_digest=request.request_digest,
        deciding_principal_id=FOUNDER,
        decision=DecisionKind.APPROVED,
    )
    assert first == second
    decided = [e for e in _events(tmp_path) if e["type"] == "approval_decided"]
    assert len(decided) == 1


def test_ac5_err_expired_request_cannot_be_decided(
    store: EffectStore, clock: list[_dt.datetime]
) -> None:
    request = _request()
    store.submit(request)
    clock[0] = request.expires_at

    with pytest.raises(RefusedError) as excinfo:
        store.decide(
            request_digest=request.request_digest,
            deciding_principal_id=FOUNDER,
            decision=DecisionKind.APPROVED,
        )
    assert excinfo.value.refusal.reason is RefusalReason.EXPIRED


# ── AC-A6-INV: lifecycle facts are not collapsed ────────────────────────────


def test_ac6_inv_each_transition_emits_its_own_bound_event(
    store: EffectStore, tmp_path: Path
) -> None:
    request = _request()
    digest = _approve(store, request)
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.EXECUTING)
    store.settle(idempotency_key="key-1", state=EffectState.ACKNOWLEDGED, external_ref="msg-9")

    events = _events(tmp_path)
    assert [e["type"] for e in events] == [
        "approval_requested",
        "approval_decided",
        "effect_state_changed",
        "effect_state_changed",
    ]
    for event in events:
        assert event["source"] == "approvals"
        assert event["data"]["work_order_id"] == request.work_order_id
        assert event["data"]["attempt_id"] == request.attempt_id
        assert event["data"]["effect_id"] == request.effect_id
        assert event["data"]["request_digest"] == digest

    states = [e["data"]["state"] for e in events if e["type"] == "effect_state_changed"]
    assert states == ["executing", "acknowledged"]


def test_ac6_inv_approval_alone_emits_no_execution_evidence(
    store: EffectStore, tmp_path: Path
) -> None:
    _approve(store, _request())
    assert [e["type"] for e in _events(tmp_path)] == [
        "approval_requested",
        "approval_decided",
    ]


def test_ac6_inv_every_emitted_event_validates_against_the_shared_schema(
    store: EffectStore, tmp_path: Path
) -> None:
    from fno import events as events_mod

    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)

    emitted = _events(tmp_path)
    assert emitted
    for event in emitted:
        events_mod.validate(event)


@pytest.mark.parametrize(
    "state,expected",
    [
        (EffectState.ACKNOWLEDGED, EvidenceResult.PASSED),
        (EffectState.FAILED, EvidenceResult.FAILED),
        (EffectState.BLOCKED, EvidenceResult.BLOCKED),
        (EffectState.UNKNOWN, EvidenceResult.UNKNOWN),
    ],
)
def test_ac6_inv_delivery_reads_facts_without_an_aggregate_verdict(
    store: EffectStore, state: EffectState, expected: EvidenceResult
) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    attempt = store.settle(idempotency_key="key-1", state=state)

    evidence = evidence_projection(attempt)
    assert evidence.result is expected
    assert evidence.subject_kind is EvidenceSubjectKind.EFFECT
    assert evidence.subject_id == attempt.effect_id
    assert effect_ref_projection(attempt).approval_id == digest


def test_ac6_inv_acknowledged_effect_is_immutable(store: EffectStore) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.ACKNOWLEDGED, external_ref="msg-9")

    with pytest.raises(RefusedError) as excinfo:
        store.settle(idempotency_key="key-1", state=EffectState.FAILED)
    assert excinfo.value.refusal.reason is RefusalReason.TERMINAL_STATE

    with pytest.raises(RefusedError):
        store.authorize_retry(idempotency_key="key-1", adapter=IDEMPOTENT_ADAPTER)


def test_ac6_inv_acknowledged_effect_is_never_redispatched(store: EffectStore) -> None:
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    store.settle(idempotency_key="key-1", state=EffectState.ACKNOWLEDGED, external_ref="msg-9")

    again = store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
    assert again.may_dispatch is False
    assert again.attempt.state is EffectState.ACKNOWLEDGED


# ── AC-A7-REC: event-write failure does not replay the effect ───────────────


def test_ac7_rec_commit_survives_event_append_failure(
    store: EffectStore, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno import events as events_mod

    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)

    def _boom(*_args: object, **_kwargs: object) -> None:
        raise OSError("events.jsonl unavailable")

    monkeypatch.setattr(events_mod, "append_event", _boom)
    attempt = store.settle(
        idempotency_key="key-1", state=EffectState.ACKNOWLEDGED, external_ref="msg-9"
    )

    # The store is authoritative even though the event never landed.
    assert attempt.state is EffectState.ACKNOWLEDGED
    assert store.get_attempt("key-1").state is EffectState.ACKNOWLEDGED
    assert store.owed_events() == 1
    assert not [e for e in _events(tmp_path) if e["type"] == "effect_state_changed"]


def test_ac7_rec_repair_re_emits_without_redispatching(
    store: EffectStore, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno import events as events_mod

    real_append = events_mod.append_event
    digest = _approve(store, _request())
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)

    monkeypatch.setattr(events_mod, "append_event", lambda *a, **k: (_ for _ in ()).throw(OSError))
    store.settle(idempotency_key="key-1", state=EffectState.ACKNOWLEDGED, external_ref="msg-9")
    monkeypatch.setattr(events_mod, "append_event", real_append)

    assert store.repair() == 1
    assert store.owed_events() == 0

    settled = [e for e in _events(tmp_path) if e["type"] == "effect_state_changed"]
    assert len(settled) == 1
    assert settled[0]["data"]["state"] == "acknowledged"

    # Repair is idempotent and never reopens the effect for dispatch.
    assert store.repair() == 0
    assert store.get_attempt("key-1").state is EffectState.ACKNOWLEDGED
    assert store.prepare(
        request_digest=digest, idempotency_key="key-1", adapter=ADAPTER
    ).may_dispatch is False


def test_ac7_rec_repair_after_reopening_the_store(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno import events as events_mod

    db = tmp_path / "approvals.db"
    events_path = tmp_path / "events.jsonl"
    request = _request()

    with EffectStore(
        db, authority=StubAuthority(), events_path=events_path, now=lambda: NOW
    ) as first:
        digest = _approve(first, request)
        first.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)
        monkeypatch.setattr(
            events_mod, "append_event", lambda *a, **k: (_ for _ in ()).throw(OSError)
        )
        first.settle(idempotency_key="key-1", state=EffectState.ACKNOWLEDGED)
    monkeypatch.undo()

    # A crashed process leaves the debt on disk; a fresh one pays it.
    with EffectStore(
        db, authority=StubAuthority(), events_path=events_path, now=lambda: NOW
    ) as second:
        assert second.owed_events() == 1
        assert second.repair() == 1
        assert second.get_attempt("key-1").state is EffectState.ACKNOWLEDGED


# ── AC-A8-INV: effect policy is function-agnostic ───────────────────────────


@pytest.mark.parametrize(
    "effect_class,expected",
    [
        ("communication.external", EffectDisposition.REQUIRE_APPROVAL),
        ("publication.public", EffectDisposition.REQUIRE_APPROVAL),
        ("system_of_record.mutation", EffectDisposition.REQUIRE_APPROVAL),
        ("something.brand.new", EffectDisposition.REQUIRE_APPROVAL),
        ("internal.draft", EffectDisposition.ALLOW),
        ("internal.research", EffectDisposition.ALLOW),
        ("financial.payment", EffectDisposition.DENY),
        ("financial.commitment", EffectDisposition.DENY),
        ("signature.contract", EffectDisposition.DENY),
        ("employment.action", EffectDisposition.DENY),
        ("infrastructure.destructive", EffectDisposition.DENY),
    ],
)
def test_ac8_inv_classification_reads_only_the_effect_class(
    effect_class: str, expected: EffectDisposition
) -> None:
    assert classify_effect(effect_class) is expected


@pytest.mark.parametrize("function", ["marketing", "support", "sales", "operations", "design"])
def test_ac8_inv_same_class_and_destination_bind_identically_across_functions(
    store: EffectStore, function: str
) -> None:
    """Only the work-order identity differs; the digest inputs do not name a function."""
    request = _request(work_order_id=f"x-{function}", request_id=f"req-{function}")
    digest = _approve(store, request)
    result = store.prepare(
        request_digest=digest, idempotency_key=f"key-{function}", adapter=ADAPTER
    )
    assert result.may_dispatch is True
    assert result.attempt.effect_class == "communication.external"


def test_ac8_inv_denied_class_never_becomes_a_pending_request(store: EffectStore) -> None:
    with pytest.raises(RefusedError) as excinfo:
        store.submit(_request(effect_class="financial.payment"))
    assert excinfo.value.refusal.reason is RefusalReason.DENIED_EFFECT_CLASS
    assert store.list_requests() == []


# ── AC-A9-ERR: conflicting idempotency bindings are refused ─────────────────


@pytest.mark.parametrize(
    "field,value",
    [
        ("action_digest", action_digest({"subject": "hello", "body": "DIFFERENT"})),
        ("destination", "email:other@example.com"),
        ("effect_class", "publication.public"),
        ("work_order_id", "x-2222"),
        ("attempt_id", "attempt-9"),
    ],
)
def test_ac9_err_a_reused_key_with_different_binding_is_refused(
    store: EffectStore, field: str, value: str
) -> None:
    first = _request()
    first_digest = _approve(store, first)
    store.prepare(request_digest=first_digest, idempotency_key="shared-key", adapter=ADAPTER)

    second = _request(request_id="req-2", **{field: value})
    second_digest = _approve(store, second)

    with pytest.raises(RefusedError) as excinfo:
        store.prepare(
            request_digest=second_digest, idempotency_key="shared-key", adapter=ADAPTER
        )

    refusal = excinfo.value.refusal
    assert refusal.reason is RefusalReason.CONFLICTING_BINDING
    assert field in refusal.fields

    # The original attempt is untouched: not mutated, not superseded, not dispatched.
    existing = store.get_attempt("shared-key")
    assert existing is not None
    assert existing.request_digest == first_digest
    assert existing.state is EffectState.PREPARED


def test_ac9_err_conflict_does_not_regrant_dispatch(store: EffectStore) -> None:
    first_digest = _approve(store, _request())
    store.prepare(request_digest=first_digest, idempotency_key="shared-key", adapter=ADAPTER)

    second = _request(request_id="req-2", destination="email:other@example.com")
    second_digest = _approve(store, second)
    with pytest.raises(RefusedError):
        store.prepare(
            request_digest=second_digest, idempotency_key="shared-key", adapter=ADAPTER
        )

    assert (
        store.prepare(
            request_digest=first_digest, idempotency_key="shared-key", adapter=ADAPTER
        ).may_dispatch
        is False
    )


# ── model invariants ────────────────────────────────────────────────────────


def test_request_rejects_a_naive_timestamp() -> None:
    with pytest.raises(ValueError):
        _request(created_at=_dt.datetime(2026, 8, 2, 12, 0))


def test_request_rejects_an_expiration_before_creation() -> None:
    with pytest.raises(ValueError):
        _request(expires_at=NOW - _dt.timedelta(hours=1))


def test_adapter_capabilities_default_to_unproven() -> None:
    adapter = AdapterCapability(adapter_id="a", adapter_version="1")
    assert adapter.remote_idempotency is False
    assert adapter.reconciliation is False
