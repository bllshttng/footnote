"""Multi-process proof that one approved effect gets one local dispatcher.

The unit suite exercises the store through two connections in one process. That
proves the SQL is right but not that the seam holds across real processes, which
is the actual acceptance contract, so these tests drive the production
``EffectStore`` entrypoint from ``multiprocessing.Process`` children racing on
one database file. Threads would not do: the exclusion we exercise is SQLite's
cross-process write lock, and threads in one process share a connection pool and
would produce an unrealistically clean result.

Lives in ``tests/integration/`` rather than a new ``tests/journeys/`` tree
because ``test_claims_concurrency.py`` already establishes this exact shape here.
"""

from __future__ import annotations

import datetime as _dt
import multiprocessing as mp
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
    action_digest,
)

FOUNDER = "principal:founder"
NOW = _dt.datetime(2026, 8, 2, 12, 0, tzinfo=_dt.timezone.utc)
ADAPTER = AdapterCapability(adapter_id="smtp", adapter_version="1")
TRIALS = 5
WORKERS = 6


class AllowFounder:
    """Independent policy stand-in. Module-level so spawned children can import it."""

    source = "integration-policy"

    def may_approve(self, *, principal_id: str, effect_class: str, destination: str) -> bool:
        return principal_id == FOUNDER


def _now() -> _dt.datetime:
    return NOW


def _request(**overrides: object) -> ApprovalRequest:
    fields: dict[str, object] = {
        "request_id": "req-1",
        "principal_id": FOUNDER,
        "work_order_id": "x-1111",
        "attempt_id": "attempt-1",
        "effect_id": "effect-1",
        "effect_class": "communication.external",
        "destination": "email:customer@example.com",
        "action_digest": action_digest({"body": "original"}),
        "created_at": NOW,
        "expires_at": NOW + _dt.timedelta(hours=1),
    }
    fields.update(overrides)
    return ApprovalRequest(**fields)  # type: ignore[arg-type]


def _open(db: Path, events: Path) -> EffectStore:
    return EffectStore(db, authority=AllowFounder(), events_path=events, now=_now)


def _seed(db: Path, events: Path, request: ApprovalRequest) -> str:
    with _open(db, events) as store:
        store.submit(request)
        store.decide(
            request_digest=request.request_digest,
            deciding_principal_id=FOUNDER,
            decision=DecisionKind.APPROVED,
        )
    return request.request_digest


def _prepare_worker(
    db_str: str, events_str: str, digest: str, key: str, barrier, queue
) -> None:
    """Child process: cross the atomic seam at the same instant as its siblings."""
    try:
        store = _open(Path(db_str), Path(events_str))
    except Exception as exc:  # pragma: no cover - setup failure surfaces as an error
        queue.put(("error", repr(exc)))
        return
    try:
        barrier.wait(timeout=30)
        result = store.prepare(request_digest=digest, idempotency_key=key, adapter=ADAPTER)
        queue.put(("ok", result.may_dispatch, result.attempt.effect_id, result.attempt.state.value))
    except RefusedError as exc:
        queue.put(("refused", exc.refusal.reason.value))
    except Exception as exc:
        queue.put(("error", repr(exc)))
    finally:
        store.close()


def _race(db: Path, events: Path, digest: str, key: str, workers: int) -> list[tuple]:
    ctx = mp.get_context("spawn")
    barrier = ctx.Barrier(workers)
    queue = ctx.Queue()
    procs = [
        ctx.Process(
            target=_prepare_worker,
            args=(str(db), str(events), digest, key, barrier, queue),
        )
        for _ in range(workers)
    ]
    for proc in procs:
        proc.start()
    for proc in procs:
        proc.join(timeout=60)
        assert proc.exitcode == 0, f"worker exited {proc.exitcode}"
    return [queue.get(timeout=10) for _ in range(workers)]


@pytest.mark.parametrize("trial", range(TRIALS))
def test_ac3_con_exactly_one_process_may_dispatch(tmp_path: Path, trial: int) -> None:
    db = tmp_path / "approvals.db"
    events = tmp_path / "events.jsonl"
    digest = _seed(db, events, _request())

    outcomes = _race(db, events, digest, "key-1", WORKERS)

    assert all(outcome[0] == "ok" for outcome in outcomes), outcomes
    assert sum(1 for outcome in outcomes if outcome[1]) == 1
    # Every process observed the same durable attempt identity.
    assert {outcome[2] for outcome in outcomes} == {"effect-1"}

    with _open(db, events) as store:
        attempt = store.get_attempt("key-1")
        assert attempt is not None
        assert attempt.request_digest == digest
        assert attempt.state is EffectState.PREPARED


def test_ac3_con_a_settled_effect_grants_no_further_dispatch(tmp_path: Path) -> None:
    db = tmp_path / "approvals.db"
    events = tmp_path / "events.jsonl"
    digest = _seed(db, events, _request())

    with _open(db, events) as store:
        assert store.prepare(
            request_digest=digest, idempotency_key="key-1", adapter=ADAPTER
        ).may_dispatch
        store.settle(idempotency_key="key-1", state=EffectState.ACKNOWLEDGED, external_ref="m-1")

    outcomes = _race(db, events, digest, "key-1", WORKERS)
    assert all(outcome[0] == "ok" for outcome in outcomes), outcomes
    assert not any(outcome[1] for outcome in outcomes)
    assert {outcome[3] for outcome in outcomes} == {"acknowledged"}


def test_ac9_err_a_conflicting_binding_is_refused_across_processes(tmp_path: Path) -> None:
    db = tmp_path / "approvals.db"
    events = tmp_path / "events.jsonl"
    first = _request()
    first_digest = _seed(db, events, first)

    with _open(db, events) as store:
        store.prepare(request_digest=first_digest, idempotency_key="shared", adapter=ADAPTER)

    second = _request(request_id="req-2", destination="email:someone-else@example.com")
    second_digest = _seed(db, events, second)

    outcomes = _race(db, events, second_digest, "shared", WORKERS)
    assert all(outcome[0] == "refused" for outcome in outcomes), outcomes
    assert {outcome[1] for outcome in outcomes} == {RefusalReason.CONFLICTING_BINDING.value}

    with _open(db, events) as store:
        attempt = store.get_attempt("shared")
        assert attempt is not None
        assert attempt.request_digest == first_digest
        assert attempt.destination == "email:customer@example.com"
        assert attempt.state is EffectState.PREPARED


def test_ac7_rec_a_crashed_process_leaves_a_repayable_event_debt(tmp_path: Path) -> None:
    """A killed process cannot drain its outbox; the next process repairs it."""
    db = tmp_path / "approvals.db"
    events = tmp_path / "events.jsonl"
    digest = _seed(db, events, _request())

    ctx = mp.get_context("spawn")
    proc = ctx.Process(target=_settle_then_die, args=(str(db), str(events), digest))
    proc.start()
    proc.join(timeout=60)
    assert proc.exitcode != 0, "worker was expected to die before draining its outbox"

    with _open(db, events) as store:
        attempt = store.get_attempt("key-1")
        assert attempt is not None
        assert attempt.state is EffectState.ACKNOWLEDGED
        assert store.owed_events() == 1

        assert store.repair() == 1
        assert store.owed_events() == 0
        # Repair never reopens the effect for dispatch.
        assert not store.prepare(
            request_digest=digest, idempotency_key="key-1", adapter=ADAPTER
        ).may_dispatch


def _settle_then_die(db_str: str, events_str: str, digest: str) -> None:
    """Commit an acknowledgment, then die before the event can be emitted."""
    import os

    from fno import events as events_mod

    store = _open(Path(db_str), Path(events_str))
    store.prepare(request_digest=digest, idempotency_key="key-1", adapter=ADAPTER)

    def _die(*_args: object, **_kwargs: object) -> None:
        os._exit(9)

    events_mod.append_event = _die  # type: ignore[assignment]
    store.settle(idempotency_key="key-1", state=EffectState.ACKNOWLEDGED, external_ref="m-1")
