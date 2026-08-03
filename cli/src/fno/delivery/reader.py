"""Read one coherent event snapshot and invoke the canonical evaluator."""

from __future__ import annotations

import hashlib
import json
import os
from datetime import datetime, timezone
from pathlib import Path

from pydantic import ValidationError

from fno.delivery.adapters import adapt_delivery_event, reject_delivery_event
from fno.delivery.contracts import (
    DeliveryEvaluateResponse,
    DeliveryEvidenceObservedEvent,
    DeliveryEvidenceRejection,
)
from fno.delivery.evaluator import evaluate_delivery
from fno.plan._doc import load_plan
from fno.plan.schema import PlanFrontmatter


def evaluate_plan_delivery(plan_path: Path, events_path: Path) -> DeliveryEvaluateResponse:
    """Evaluate an activated plan from one immutable journal byte snapshot."""
    try:
        plan = PlanFrontmatter.model_validate(load_plan(plan_path).frontmatter)
    except (OSError, ValueError, ValidationError) as exc:
        return _undeterminable(f"plan is unreadable or invalid: {exc}")
    if plan.completion != "delivery":
        return DeliveryEvaluateResponse(
            status="inactive",
            diagnostics=("generic delivery completion is not activated",),
        )
    company_work = plan.company_work
    if company_work is None or company_work.work_order is None:
        return _inactive_declaration()
    if not company_work.deliverables:
        return _inactive_declaration()
    required = tuple(
        evidence_id
        for deliverable in company_work.deliverables
        for evidence_id in deliverable.required_evidence_ids
    )
    if not required:
        return _inactive_declaration()

    try:
        raw = _read_coherent_bytes(events_path)
        events = _parse_events(raw)
    except JournalRevisionConflict as exc:
        fact_revision = "conflict:" + ":".join(exc.revisions)
        diagnostic = (
            "event journal changed during read; conflicting revisions "
            + ", ".join(exc.revisions)
        )
        verdict = evaluate_delivery(
            company_work,
            tuple(
                DeliveryEvidenceRejection(
                    evidence_id=evidence_id,
                    producer="event-journal",
                    diagnostic=f"requirement {evidence_id}: {diagnostic}",
                    fact_revision=fact_revision,
                )
                for evidence_id in required
            ),
            evaluated_at=datetime.now(timezone.utc),
        )
        verdict = verdict.model_copy(update={"diagnostics": (diagnostic,)})
        return DeliveryEvaluateResponse(
            status="evaluated",
            fact_revision=fact_revision,
            verdict=verdict,
            diagnostics=verdict.diagnostics,
        )
    except (OSError, ValueError) as exc:
        return _undeterminable(f"event journal is unreadable or malformed: {exc}")
    fact_revision = f"sha256:{hashlib.sha256(raw).hexdigest()}"
    evaluated_at = datetime.now(timezone.utc)
    try:
        facts = tuple(
            item
            for event, request_event in _current_evidence_events(events)
            for item in _normalize_event(
                company_work,
                event,
                request_event=request_event,
                evaluated_at=evaluated_at,
                fact_revision=fact_revision,
            )
        )
    except ValueError as exc:
        return _undeterminable(str(exc))
    verdict = evaluate_delivery(company_work, facts, evaluated_at=evaluated_at)
    return DeliveryEvaluateResponse(
        status="evaluated",
        fact_revision=fact_revision,
        verdict=verdict,
        diagnostics=verdict.diagnostics,
    )


class JournalRevisionConflict(OSError):
    def __init__(self, *revisions: str) -> None:
        super().__init__("event journal changed during read")
        self.revisions = tuple(revisions)


def _stat_revision(stat: os.stat_result) -> str:
    return f"stat:{stat.st_dev}:{stat.st_ino}:{stat.st_size}:{stat.st_mtime_ns}"


def _read_coherent_bytes(path: Path, *, after_read=None) -> bytes:
    with path.open("rb") as stream:
        before = os.fstat(stream.fileno())
        raw = stream.read()
        if after_read is not None:
            after_read()
        after = os.fstat(stream.fileno())
    current = path.stat()
    identity = lambda stat: (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns)
    if identity(before) != identity(after) or identity(after) != identity(current):
        raise JournalRevisionConflict(
            _stat_revision(before),
            _stat_revision(after),
            _stat_revision(current),
        )
    if len(raw) != after.st_size:
        raise OSError("event journal read was partial")
    return raw


def _parse_events(raw: bytes) -> tuple[dict[str, object], ...]:
    events: list[dict[str, object]] = []
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ValueError("journal is not UTF-8") from exc
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError as exc:
            raise ValueError(f"malformed JSON at line {line_number}") from exc
        if not isinstance(event, dict):
            raise ValueError(f"event at line {line_number} is not an object")
        events.append(event)
    return tuple(events)


def _current_evidence_events(
    events: tuple[dict[str, object], ...],
) -> tuple[tuple[dict[str, object], dict[str, object] | None], ...]:
    """Select current producer state without reconstructing its authority."""
    observed: dict[tuple[str, ...], tuple[str, list[dict[str, object]]]] = {}
    requests: dict[str, dict[str, object]] = {}
    approvals: dict[str, dict[str, object]] = {}
    effects: dict[str, dict[str, object]] = {}
    for event in events:
        event_type = event.get("type")
        data = event.get("data")
        if event_type == "delivery_evidence_observed":
            try:
                parsed = DeliveryEvidenceObservedEvent.model_validate(event)
                fact = parsed.data
                evidence = fact.evidence
                identity = (
                    evidence.work_order_id,
                    evidence.attempt_id,
                    evidence.id,
                    evidence.subject_kind.value,
                    evidence.subject_id,
                    fact.producer,
                )
                revision = fact.fact_revision
            except ValidationError:
                evidence = data.get("evidence") if isinstance(data, dict) else None
                evidence_id = evidence.get("id") if isinstance(evidence, dict) else None
                producer = data.get("producer") if isinstance(data, dict) else None
                if not isinstance(evidence_id, str):
                    raise ValueError("malformed delivery_evidence_observed event")
                identity = ("rejected", evidence_id, str(producer))
                revision = "rejected"
            current = observed.get(identity)
            if current is None or current[0] != revision:
                observed[identity] = (revision, [event])
            else:
                current[1].append(event)
            continue
        if not isinstance(data, dict):
            continue
        if event_type == "approval_requested":
            digest = data.get("request_digest")
            if isinstance(digest, str) and digest:
                requests[digest] = event
        elif event_type == "approval_decided":
            digest = data.get("request_digest")
            if isinstance(digest, str) and digest:
                approvals[digest] = event
        elif event_type == "effect_state_changed":
            key = data.get("idempotency_key")
            effect_id = data.get("effect_id")
            if isinstance(key, str) and key:
                effects[f"key:{key}"] = event
            elif isinstance(effect_id, str) and effect_id:
                effects[f"effect:{effect_id}"] = event
    current: list[tuple[dict[str, object], dict[str, object] | None]] = [
        (event, None)
        for _, events_for_revision in observed.values()
        for event in events_for_revision
    ]
    for digest, request in requests.items():
        current.append((approvals.get(digest, request), None))
    for event in effects.values():
        data = event.get("data")
        digest = data.get("request_digest") if isinstance(data, dict) else None
        request = requests.get(digest) if isinstance(digest, str) else None
        current.append((event, request))
    return tuple(current)


def _normalize_event(
    company_work,
    event: dict[str, object],
    *,
    request_event: dict[str, object] | None,
    evaluated_at: datetime,
    fact_revision: str,
):
    if event.get("type") == "delivery_evidence_observed":
        try:
            parsed = DeliveryEvidenceObservedEvent.model_validate(event)
        except ValidationError:
            rejected = reject_delivery_event(
                company_work, event, fact_revision=fact_revision
            )
            if not rejected:
                raise ValueError("malformed delivery_evidence_observed event")
            return rejected
        if event.get("source") != "target":
            return reject_delivery_event(
                company_work, event, fact_revision=fact_revision
            )
        return (parsed.data.model_copy(update={"fact_revision": fact_revision}),)
    facts = adapt_delivery_event(
        company_work,
        event,
        fresh_until=evaluated_at,
        fact_revision=fact_revision,
        approval_request_event=request_event,
    )
    if facts:
        return facts
    return reject_delivery_event(company_work, event, fact_revision=fact_revision)


def _undeterminable(diagnostic: str) -> DeliveryEvaluateResponse:
    return DeliveryEvaluateResponse(status="undeterminable", diagnostics=(diagnostic,))


def _inactive_declaration() -> DeliveryEvaluateResponse:
    return DeliveryEvaluateResponse(
        status="inactive",
        diagnostics=("generic delivery requires a valid company work declaration",),
    )
