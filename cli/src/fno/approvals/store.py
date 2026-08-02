"""Atomic approval and effect-attempt store.

SQLite is the backend because the acceptance contract is cross-process mutual
exclusion, durable unique keys, and crash recovery. ``BEGIN IMMEDIATE`` plus a
``UNIQUE`` idempotency key give all three from the standard library; an advisory
lockfile would give none of them.

Events use a transactional outbox: the state change and the owed event commit in
one transaction, then the event is emitted and its outbox row deleted. A crash or
an append failure between those two steps leaves the store authoritative and the
event owed, so :meth:`EffectStore.repair` re-emits from the committed record and
never re-crosses the adapter boundary.
"""

from __future__ import annotations

import datetime as _dt
import json
import sqlite3
from collections.abc import Callable, Iterable
from contextlib import contextmanager
from pathlib import Path
from typing import Any

from fno.approvals.models import (
    AdapterCapability,
    ApprovalDecision,
    ApprovalRequest,
    Authority,
    DecisionKind,
    EffectAttempt,
    EffectDisposition,
    EffectState,
    PrepareResult,
    Refusal,
    RefusalReason,
    RefusedError,
    classify_effect,
    utcnow,
)

__all__ = ["EffectStore", "default_db_path"]

_SCHEMA = """
CREATE TABLE IF NOT EXISTS requests (
    request_digest TEXT PRIMARY KEY,
    request_id     TEXT NOT NULL UNIQUE,
    principal_id   TEXT NOT NULL,
    work_order_id  TEXT NOT NULL,
    attempt_id     TEXT NOT NULL,
    effect_id      TEXT NOT NULL,
    effect_class   TEXT NOT NULL,
    destination    TEXT NOT NULL,
    action_digest  TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    expires_at     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS decisions (
    request_digest       TEXT PRIMARY KEY REFERENCES requests(request_digest),
    deciding_principal_id TEXT NOT NULL,
    decision             TEXT NOT NULL,
    decided_at           TEXT NOT NULL,
    transport            TEXT
);

CREATE TABLE IF NOT EXISTS attempts (
    idempotency_key    TEXT PRIMARY KEY,
    effect_id          TEXT NOT NULL,
    work_order_id      TEXT NOT NULL,
    attempt_id         TEXT NOT NULL,
    request_digest     TEXT NOT NULL,
    action_digest      TEXT NOT NULL,
    destination        TEXT NOT NULL,
    effect_class       TEXT NOT NULL,
    adapter_id         TEXT NOT NULL,
    adapter_version    TEXT NOT NULL,
    state              TEXT NOT NULL,
    external_ref       TEXT,
    reconciliation_ref TEXT,
    dispatch_claimed   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS outbox (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    payload TEXT NOT NULL
);
"""

# Terminal states accept no further transition. UNKNOWN never reaches EXECUTING
# through settle(): only authorize_retry() may reopen it, and only against a
# proven adapter capability.
_ALLOWED_TRANSITIONS: dict[EffectState, frozenset[EffectState]] = {
    EffectState.PREPARED: frozenset(
        {
            EffectState.EXECUTING,
            EffectState.ACKNOWLEDGED,
            EffectState.FAILED,
            EffectState.BLOCKED,
            EffectState.UNKNOWN,
        }
    ),
    EffectState.EXECUTING: frozenset(
        {
            EffectState.ACKNOWLEDGED,
            EffectState.FAILED,
            EffectState.BLOCKED,
            EffectState.UNKNOWN,
        }
    ),
    EffectState.FAILED: frozenset(
        {EffectState.EXECUTING, EffectState.ACKNOWLEDGED, EffectState.FAILED, EffectState.UNKNOWN}
    ),
    EffectState.UNKNOWN: frozenset({EffectState.ACKNOWLEDGED, EffectState.FAILED}),
    EffectState.ACKNOWLEDGED: frozenset(),
    EffectState.BLOCKED: frozenset(),
}

_UNKNOWN_RECOVERY = (
    "The destination may or may not have applied this effect. Reconcile with the "
    "destination, or retry only through an adapter that enforces this idempotency "
    "key remotely. Settle to acknowledged or failed once the destination is read."
)


def default_db_path() -> Path:
    from fno import paths

    return paths.state_dir() / "approvals.db"


def _refuse(
    reason: RefusalReason,
    detail: str,
    *,
    fields: Iterable[str] = (),
    authority_source: str | None = None,
    recovery: str | None = None,
) -> None:
    raise RefusedError(
        Refusal(
            reason=reason,
            detail=detail,
            fields=tuple(fields),
            authority_source=authority_source,
            recovery=recovery,
        )
    )


class EffectStore:
    """Authoritative store for approvals and effect attempts."""

    def __init__(
        self,
        db_path: Path | str | None = None,
        *,
        authority: Authority,
        events_path: Path | None = None,
        now: Callable[[], _dt.datetime] = utcnow,
        busy_timeout_seconds: float = 30.0,
    ) -> None:
        self._path = Path(db_path) if db_path is not None else default_db_path()
        self._path.parent.mkdir(parents=True, exist_ok=True)
        self._authority = authority
        self._events_path = events_path
        self._now = now
        # isolation_level=None hands transaction control to us so BEGIN IMMEDIATE
        # is the real write barrier rather than sqlite3's implicit deferred begin.
        self._conn = sqlite3.connect(
            self._path, isolation_level=None, timeout=busy_timeout_seconds
        )
        self._conn.row_factory = sqlite3.Row
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA synchronous=FULL")
        self._conn.execute(f"PRAGMA busy_timeout={int(busy_timeout_seconds * 1000)}")
        self._conn.executescript(_SCHEMA)

    def close(self) -> None:
        self._conn.close()

    def __enter__(self) -> EffectStore:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    # -- transactions ----------------------------------------------------

    @contextmanager
    def _write(self):
        """One exclusive write transaction. Rolls back whole on any refusal."""
        self._conn.execute("BEGIN IMMEDIATE")
        try:
            yield self._conn
        except BaseException:
            self._conn.execute("ROLLBACK")
            raise
        self._conn.execute("COMMIT")

    # -- approval lifecycle ----------------------------------------------

    def submit(self, request: ApprovalRequest) -> ApprovalRequest:
        """Record one exact request. A denied effect class never becomes pending."""
        if classify_effect(request.effect_class) is EffectDisposition.DENY:
            _refuse(
                RefusalReason.DENIED_EFFECT_CLASS,
                f"effect class {request.effect_class} is denied by core policy",
                fields=["effect_class"],
                recovery="Denied classes need an explicit policy and adapter contract first.",
            )

        digest = request.request_digest
        with self._write() as conn:
            existing = conn.execute(
                "SELECT request_digest FROM requests WHERE request_digest = ?", (digest,)
            ).fetchone()
            if existing is not None:
                return request
            conn.execute(
                "INSERT INTO requests (request_digest, request_id, principal_id, work_order_id,"
                " attempt_id, effect_id, effect_class, destination, action_digest, created_at,"
                " expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                (
                    digest,
                    request.request_id,
                    request.principal_id,
                    request.work_order_id,
                    request.attempt_id,
                    request.effect_id,
                    request.effect_class,
                    request.destination,
                    request.action_digest,
                    request.created_at.isoformat(),
                    request.expires_at.isoformat(),
                ),
            )
            self._enqueue(
                conn,
                "approval_requested",
                {
                    "request_digest": digest,
                    "request_id": request.request_id,
                    "principal_id": request.principal_id,
                    "work_order_id": request.work_order_id,
                    "attempt_id": request.attempt_id,
                    "effect_id": request.effect_id,
                    "effect_class": request.effect_class,
                    "destination": request.destination,
                    "action_digest": request.action_digest,
                    "expires_at": request.expires_at.isoformat(),
                },
            )
        self._drain()
        return request

    def decide(
        self,
        *,
        request_digest: str,
        deciding_principal_id: str,
        decision: DecisionKind,
        transport: str | None = None,
    ) -> ApprovalDecision:
        """Decide one exact request digest.

        The deciding principal is authorized against the injected policy source.
        ``transport`` is recorded but never consulted for authorization, so a
        transport cannot add its caller to the authorized set.
        """
        now = self._now()
        with self._write() as conn:
            row = conn.execute(
                "SELECT * FROM requests WHERE request_digest = ?", (request_digest,)
            ).fetchone()
            if row is None:
                _refuse(
                    RefusalReason.UNKNOWN_REQUEST,
                    f"no request matches digest {request_digest}",
                    fields=["request_digest"],
                    recovery="Re-submit the request; an edited action produces a new digest.",
                )

            prior = conn.execute(
                "SELECT * FROM decisions WHERE request_digest = ?", (request_digest,)
            ).fetchone()
            if prior is not None:
                # Immutable: return the existing decision rather than minting a second.
                if (
                    prior["deciding_principal_id"] == deciding_principal_id
                    and prior["decision"] == decision.value
                ):
                    return _decision_from_row(prior)
                _refuse(
                    RefusalReason.REPLAY,
                    f"request {request_digest} was already decided "
                    f"{prior['decision']} by {prior['deciding_principal_id']}",
                    fields=["request_digest"],
                    recovery="A decided request is immutable. Submit a new request instead.",
                )

            if _parse_ts(row["expires_at"]) <= now:
                _refuse(
                    RefusalReason.EXPIRED,
                    f"request {request_digest} expired at {row['expires_at']}",
                    fields=["expires_at"],
                    recovery="Submit a new request with a fresh expiration.",
                )

            authorized = self._authority.may_approve(
                principal_id=deciding_principal_id,
                effect_class=row["effect_class"],
                destination=row["destination"],
            )
            if not authorized:
                _refuse(
                    RefusalReason.UNAUTHORIZED_PRINCIPAL,
                    f"principal {deciding_principal_id} may not decide "
                    f"{row['effect_class']} to {row['destination']}",
                    fields=["deciding_principal_id"],
                    authority_source=getattr(self._authority, "source", "unknown"),
                    recovery="Have an authorized principal decide this request.",
                )

            conn.execute(
                "INSERT INTO decisions (request_digest, deciding_principal_id, decision,"
                " decided_at, transport) VALUES (?,?,?,?,?)",
                (
                    request_digest,
                    deciding_principal_id,
                    decision.value,
                    now.isoformat(),
                    transport,
                ),
            )
            self._enqueue(
                conn,
                "approval_decided",
                {
                    "request_digest": request_digest,
                    "decision": decision.value,
                    "deciding_principal_id": deciding_principal_id,
                    "work_order_id": row["work_order_id"],
                    "attempt_id": row["attempt_id"],
                    "effect_id": row["effect_id"],
                    "authority_source": getattr(self._authority, "source", "unknown"),
                    "transport": transport,
                },
            )
            result = ApprovalDecision(
                request_digest=request_digest,
                deciding_principal_id=deciding_principal_id,
                decision=decision,
                decided_at=now,
                transport=transport,
            )
        self._drain()
        return result

    # -- effect journal --------------------------------------------------

    def prepare(
        self,
        *,
        request_digest: str,
        idempotency_key: str,
        adapter: AdapterCapability,
    ) -> PrepareResult:
        """Atomically create or read one effect attempt.

        Concurrent identical calls observe the same durable attempt, and exactly
        one receives ``may_dispatch=True``.
        """
        now = self._now()
        with self._write() as conn:
            request = self._authorized_request(conn, request_digest, now)

            existing = conn.execute(
                "SELECT * FROM attempts WHERE idempotency_key = ?", (idempotency_key,)
            ).fetchone()
            if existing is not None:
                conflicts = [
                    field
                    for field, value in (
                        ("action_digest", request["action_digest"]),
                        ("destination", request["destination"]),
                        ("effect_class", request["effect_class"]),
                        ("work_order_id", request["work_order_id"]),
                        ("attempt_id", request["attempt_id"]),
                    )
                    if existing[field] != value
                ]
                if conflicts:
                    _refuse(
                        RefusalReason.CONFLICTING_BINDING,
                        f"idempotency key {idempotency_key} is already bound to a "
                        f"different {', '.join(conflicts)}",
                        fields=conflicts,
                        recovery="Use a distinct idempotency key for a distinct effect.",
                    )
                attempt = _attempt_from_row(existing)
                if attempt.state in (EffectState.ACKNOWLEDGED, EffectState.BLOCKED):
                    return PrepareResult(attempt=attempt, may_dispatch=False)
                claimed = conn.execute(
                    "UPDATE attempts SET dispatch_claimed = 1 WHERE idempotency_key = ?"
                    " AND dispatch_claimed = 0",
                    (idempotency_key,),
                ).rowcount
                return PrepareResult(attempt=attempt, may_dispatch=claimed == 1)

            conn.execute(
                "INSERT INTO attempts (idempotency_key, effect_id, work_order_id, attempt_id,"
                " request_digest, action_digest, destination, effect_class, adapter_id,"
                " adapter_version, state, dispatch_claimed) VALUES (?,?,?,?,?,?,?,?,?,?,?,1)",
                (
                    idempotency_key,
                    request["effect_id"],
                    request["work_order_id"],
                    request["attempt_id"],
                    request_digest,
                    request["action_digest"],
                    request["destination"],
                    request["effect_class"],
                    adapter.adapter_id,
                    adapter.adapter_version,
                    EffectState.PREPARED.value,
                ),
            )
            row = conn.execute(
                "SELECT * FROM attempts WHERE idempotency_key = ?", (idempotency_key,)
            ).fetchone()
            result = PrepareResult(attempt=_attempt_from_row(row), may_dispatch=True)
        return result

    def settle(
        self,
        *,
        idempotency_key: str,
        state: EffectState,
        external_ref: str | None = None,
        reconciliation_ref: str | None = None,
    ) -> EffectAttempt:
        """Record one honest outcome for an attempt.

        An ambiguous outcome settles to ``unknown``, never to failed or
        acknowledged.
        """
        with self._write() as conn:
            row = conn.execute(
                "SELECT * FROM attempts WHERE idempotency_key = ?", (idempotency_key,)
            ).fetchone()
            if row is None:
                _refuse(
                    RefusalReason.UNKNOWN_REQUEST,
                    f"no effect attempt for idempotency key {idempotency_key}",
                    fields=["idempotency_key"],
                )

            current = EffectState(row["state"])
            if state not in _ALLOWED_TRANSITIONS[current]:
                _refuse(
                    RefusalReason.TERMINAL_STATE
                    if not _ALLOWED_TRANSITIONS[current]
                    else RefusalReason.UNSAFE_RETRY,
                    f"effect {idempotency_key} cannot move from {current.value} to {state.value}",
                    fields=["state"],
                    recovery=_UNKNOWN_RECOVERY if current is EffectState.UNKNOWN else None,
                )

            conn.execute(
                "UPDATE attempts SET state = ?,"
                " external_ref = COALESCE(?, external_ref),"
                " reconciliation_ref = COALESCE(?, reconciliation_ref)"
                " WHERE idempotency_key = ?",
                (state.value, external_ref, reconciliation_ref, idempotency_key),
            )
            updated = _attempt_from_row(
                conn.execute(
                    "SELECT * FROM attempts WHERE idempotency_key = ?", (idempotency_key,)
                ).fetchone()
            )
            self._enqueue(
                conn,
                "effect_state_changed",
                {
                    "idempotency_key": idempotency_key,
                    "state": state.value,
                    "previous_state": current.value,
                    "request_digest": updated.request_digest,
                    "work_order_id": updated.work_order_id,
                    "attempt_id": updated.attempt_id,
                    "effect_id": updated.effect_id,
                    "external_ref": updated.external_ref,
                    "reconciliation_ref": updated.reconciliation_ref,
                },
            )
        self._drain()
        return updated

    def authorize_retry(
        self, *, idempotency_key: str, adapter: AdapterCapability
    ) -> EffectAttempt:
        """Reopen an ambiguous attempt only when a retry is provably safe.

        Safe means the adapter enforces this idempotency key remotely, or a
        reconciliation read can prove whether the effect already happened.
        Without either, the retry is refused: Footnote guarantees one local
        dispatcher but makes no external exactly-once claim.
        """
        with self._write() as conn:
            row = conn.execute(
                "SELECT * FROM attempts WHERE idempotency_key = ?", (idempotency_key,)
            ).fetchone()
            if row is None:
                _refuse(
                    RefusalReason.UNKNOWN_REQUEST,
                    f"no effect attempt for idempotency key {idempotency_key}",
                    fields=["idempotency_key"],
                )

            current = EffectState(row["state"])
            if current in (EffectState.ACKNOWLEDGED, EffectState.BLOCKED):
                _refuse(
                    RefusalReason.TERMINAL_STATE,
                    f"effect {idempotency_key} is already {current.value}",
                    fields=["state"],
                )
            if current is EffectState.UNKNOWN and not (
                adapter.remote_idempotency or adapter.reconciliation
            ):
                _refuse(
                    RefusalReason.UNSAFE_RETRY,
                    f"adapter {adapter.adapter_id} proves neither remote idempotency nor "
                    f"reconciliation, so retrying an unknown effect may duplicate it",
                    fields=["remote_idempotency", "reconciliation"],
                    recovery=_UNKNOWN_RECOVERY,
                )

            conn.execute(
                "UPDATE attempts SET state = ?, dispatch_claimed = 1,"
                " adapter_id = ?, adapter_version = ? WHERE idempotency_key = ?",
                (
                    EffectState.EXECUTING.value,
                    adapter.adapter_id,
                    adapter.adapter_version,
                    idempotency_key,
                ),
            )
            updated = _attempt_from_row(
                conn.execute(
                    "SELECT * FROM attempts WHERE idempotency_key = ?", (idempotency_key,)
                ).fetchone()
            )
            self._enqueue(
                conn,
                "effect_state_changed",
                {
                    "idempotency_key": idempotency_key,
                    "state": EffectState.EXECUTING.value,
                    "previous_state": current.value,
                    "request_digest": updated.request_digest,
                    "work_order_id": updated.work_order_id,
                    "attempt_id": updated.attempt_id,
                    "effect_id": updated.effect_id,
                },
            )
        self._drain()
        return updated

    # -- reads -----------------------------------------------------------

    def get_request(self, request_digest: str) -> ApprovalRequest | None:
        row = self._conn.execute(
            "SELECT * FROM requests WHERE request_digest = ?", (request_digest,)
        ).fetchone()
        return _request_from_row(row) if row is not None else None

    def get_decision(self, request_digest: str) -> ApprovalDecision | None:
        row = self._conn.execute(
            "SELECT * FROM decisions WHERE request_digest = ?", (request_digest,)
        ).fetchone()
        return _decision_from_row(row) if row is not None else None

    def get_attempt(self, idempotency_key: str) -> EffectAttempt | None:
        row = self._conn.execute(
            "SELECT * FROM attempts WHERE idempotency_key = ?", (idempotency_key,)
        ).fetchone()
        return _attempt_from_row(row) if row is not None else None

    def list_requests(self, *, pending_only: bool = False) -> list[ApprovalRequest]:
        sql = "SELECT r.* FROM requests r"
        if pending_only:
            sql += " LEFT JOIN decisions d ON d.request_digest = r.request_digest"
            sql += " WHERE d.request_digest IS NULL"
        sql += " ORDER BY r.created_at"
        return [_request_from_row(row) for row in self._conn.execute(sql)]

    # -- event outbox ----------------------------------------------------

    def _enqueue(self, conn: sqlite3.Connection, event_type: str, data: dict[str, Any]) -> None:
        """Owe an event inside the caller's transaction. Committed with the state."""
        payload = {
            "ts": self._now().strftime("%Y-%m-%dT%H:%M:%SZ"),
            "type": event_type,
            "source": "approvals",
            "data": data,
        }
        conn.execute("INSERT INTO outbox (payload) VALUES (?)", (json.dumps(payload),))

    def _drain(self) -> int:
        """Emit owed events. A failure here leaves the debt for :meth:`repair`."""
        from fno import events as events_mod

        emitted = 0
        rows = self._conn.execute("SELECT seq, payload FROM outbox ORDER BY seq").fetchall()
        for row in rows:
            try:
                events_mod.append_event(json.loads(row["payload"]), self._events_path)
            except Exception:
                # The store already committed. Stop draining and keep the debt so
                # recovery re-emits it without redispatching the external effect.
                break
            self._conn.execute("DELETE FROM outbox WHERE seq = ?", (row["seq"],))
            emitted += 1
        return emitted

    def repair(self) -> int:
        """Re-emit every event owed by a committed record. Never dispatches."""
        return self._drain()

    def owed_events(self) -> int:
        return int(self._conn.execute("SELECT COUNT(*) FROM outbox").fetchone()[0])

    # -- internals -------------------------------------------------------

    def _authorized_request(
        self, conn: sqlite3.Connection, request_digest: str, now: _dt.datetime
    ) -> sqlite3.Row:
        """Re-validate the bound request at execution time, not only at decision time."""
        row = conn.execute(
            "SELECT * FROM requests WHERE request_digest = ?", (request_digest,)
        ).fetchone()
        if row is None:
            _refuse(
                RefusalReason.DIGEST_MISMATCH,
                f"no approved request matches digest {request_digest}",
                fields=["request_digest"],
                recovery="Any change to content, destination, class, attempt, principal, "
                "or expiration requires a new request and a new decision.",
            )

        decision = conn.execute(
            "SELECT * FROM decisions WHERE request_digest = ?", (request_digest,)
        ).fetchone()
        if decision is None:
            _refuse(
                RefusalReason.NOT_APPROVED,
                f"request {request_digest} has no decision",
                fields=["request_digest"],
            )
        if decision["decision"] != DecisionKind.APPROVED.value:
            _refuse(
                RefusalReason.DECLINED,
                f"request {request_digest} was declined by "
                f"{decision['deciding_principal_id']}",
                fields=["decision"],
                recovery="A declined request is terminal. Submit a new request.",
            )
        if _parse_ts(row["expires_at"]) <= now:
            _refuse(
                RefusalReason.EXPIRED,
                f"approval for {request_digest} expired at {row['expires_at']}",
                fields=["expires_at"],
                recovery="Submit a new request with a fresh expiration.",
            )
        if classify_effect(row["effect_class"]) is EffectDisposition.DENY:
            _refuse(
                RefusalReason.DENIED_EFFECT_CLASS,
                f"effect class {row['effect_class']} is denied by core policy",
                fields=["effect_class"],
            )
        # The approving principal must still hold authority at execution time.
        if not self._authority.may_approve(
            principal_id=decision["deciding_principal_id"],
            effect_class=row["effect_class"],
            destination=row["destination"],
        ):
            _refuse(
                RefusalReason.UNAUTHORIZED_PRINCIPAL,
                f"principal {decision['deciding_principal_id']} no longer may approve "
                f"{row['effect_class']} to {row['destination']}",
                fields=["deciding_principal_id"],
                authority_source=getattr(self._authority, "source", "unknown"),
            )
        return row


def _parse_ts(value: str) -> _dt.datetime:
    parsed = _dt.datetime.fromisoformat(value)
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=_dt.timezone.utc)
    return parsed


def _request_from_row(row: sqlite3.Row) -> ApprovalRequest:
    return ApprovalRequest(
        request_id=row["request_id"],
        principal_id=row["principal_id"],
        work_order_id=row["work_order_id"],
        attempt_id=row["attempt_id"],
        effect_id=row["effect_id"],
        effect_class=row["effect_class"],
        destination=row["destination"],
        action_digest=row["action_digest"],
        created_at=_parse_ts(row["created_at"]),
        expires_at=_parse_ts(row["expires_at"]),
    )


def _decision_from_row(row: sqlite3.Row) -> ApprovalDecision:
    return ApprovalDecision(
        request_digest=row["request_digest"],
        deciding_principal_id=row["deciding_principal_id"],
        decision=DecisionKind(row["decision"]),
        decided_at=_parse_ts(row["decided_at"]),
        transport=row["transport"],
    )


def _attempt_from_row(row: sqlite3.Row) -> EffectAttempt:
    return EffectAttempt(
        effect_id=row["effect_id"],
        work_order_id=row["work_order_id"],
        attempt_id=row["attempt_id"],
        request_digest=row["request_digest"],
        idempotency_key=row["idempotency_key"],
        action_digest=row["action_digest"],
        destination=row["destination"],
        effect_class=row["effect_class"],
        adapter_id=row["adapter_id"],
        adapter_version=row["adapter_version"],
        state=EffectState(row["state"]),
        external_ref=row["external_ref"],
        reconciliation_ref=row["reconciliation_ref"],
    )
