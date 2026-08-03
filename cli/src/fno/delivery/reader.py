"""Read one coherent event snapshot and invoke the canonical evaluator."""

from __future__ import annotations

import hashlib
import json
import os
from datetime import datetime, timezone
from pathlib import Path

from pydantic import ValidationError

from fno.delivery.adapters import adapt_delivery_event
from fno.delivery.contracts import DeliveryEvaluateResponse, DeliveryEvidenceObservedEvent
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
        return _undeterminable("delivery activation requires company_work.work_order")
    if not company_work.deliverables:
        return _undeterminable("delivery activation requires at least one deliverable")
    required = tuple(
        evidence_id
        for deliverable in company_work.deliverables
        for evidence_id in deliverable.required_evidence_ids
    )
    if not required:
        return _undeterminable("delivery activation requires at least one required evidence slot")

    try:
        raw = _read_coherent_bytes(events_path)
        events = _parse_events(raw)
    except (OSError, ValueError) as exc:
        return _undeterminable(f"event journal is unreadable or malformed: {exc}")
    fact_revision = f"sha256:{hashlib.sha256(raw).hexdigest()}"
    evaluated_at = datetime.now(timezone.utc)
    try:
        facts = tuple(
            fact.model_copy(update={"fact_revision": fact_revision})
            for event, request_event in _current_evidence_events(events)
            for fact in adapt_delivery_event(
                company_work,
                event,
                fresh_until=evaluated_at,
                fact_revision=fact_revision,
                approval_request_event=request_event,
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


def _read_coherent_bytes(path: Path) -> bytes:
    with path.open("rb") as stream:
        before = os.fstat(stream.fileno())
        raw = stream.read()
        after = os.fstat(stream.fileno())
    current = path.stat()
    identity = lambda stat: (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns)
    if identity(before) != identity(after) or identity(after) != identity(current):
        raise OSError("event journal changed during read")
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
            except ValidationError as exc:
                raise ValueError("malformed delivery_evidence_observed event") from exc
            fact = parsed.data
            evidence = fact.evidence
            identity = (
                evidence.work_order_id,
                evidence.attempt_id,
                evidence.id,
                evidence.subject_kind.value,
                evidence.subject_id,
            )
            current = observed.get(identity)
            if current is None or current[0] != fact.fact_revision:
                observed[identity] = (fact.fact_revision, [event])
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
            if isinstance(key, str) and key:
                effects[key] = event
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


def _undeterminable(diagnostic: str) -> DeliveryEvaluateResponse:
    return DeliveryEvaluateResponse(status="undeterminable", diagnostics=(diagnostic,))
