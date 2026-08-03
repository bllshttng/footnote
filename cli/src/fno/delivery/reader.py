"""Read one coherent event snapshot and invoke the canonical evaluator."""

from __future__ import annotations

import hashlib
import json
import os
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from pydantic import ValidationError

from fno.delivery.adapters import adapt_delivery_event, reject_delivery_event
from fno.delivery.contracts import (
    DeliveryEvaluateResponse,
    DeliveryEvidenceObservedEvent,
    DeliveryEvidenceRejection,
    DeliveryVerdict,
)
from fno.delivery.evaluator import evaluate_delivery
from fno.company.contracts import CompanyWorkRefs
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
        current_events = _current_evidence_events(events, company_work)
        facts = tuple(
            item
            for event, request_event in current_events
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
    evidence_revision = _delivery_evidence_revision(verdict)
    return DeliveryEvaluateResponse(
        status="evaluated",
        fact_revision=fact_revision,
        evidence_revision=evidence_revision,
        verdict=verdict,
        diagnostics=verdict.diagnostics,
    )


class JournalRevisionConflict(OSError):
    def __init__(self, *revisions: str) -> None:
        super().__init__("event journal changed during read")
        self.revisions = tuple(revisions)


def _stat_revision(stat: os.stat_result) -> str:
    return f"stat:{stat.st_dev}:{stat.st_ino}:{stat.st_size}:{stat.st_mtime_ns}"


def _stat_identity(stat: os.stat_result) -> tuple[int, int, int, int]:
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns


def _read_coherent_bytes(path: Path, *, after_read=None) -> bytes:
    with path.open("rb") as stream:
        before = os.fstat(stream.fileno())
        raw = stream.read()
        if after_read is not None:
            after_read()
        after = os.fstat(stream.fileno())
    current = path.stat()
    if _stat_identity(before) != _stat_identity(after) or _stat_identity(
        after
    ) != _stat_identity(current):
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


def _delivery_evidence_revision(
    verdict: DeliveryVerdict,
) -> str:
    canonical = json.dumps(
        verdict.model_dump(mode="json", exclude={"fact_revision", "session_id"}),
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return f"sha256:{hashlib.sha256(canonical).hexdigest()}"


def _event_time(event: dict[str, object]) -> str:
    value = event.get("ts")
    return value if isinstance(value, str) else ""


def _event_datetime(event: dict[str, object]) -> datetime | None:
    value = _event_time(event)
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return parsed if parsed.tzinfo is not None else None


@dataclass(frozen=True)
class _LatestEvent:
    event: dict[str, object]
    timestamp: datetime | None
    latest_valid_timestamp: datetime | None


def _select_latest(
    selected: dict[str, _LatestEvent], key: str, event: dict[str, object]
) -> None:
    current = selected.get(key)
    timestamp = _event_datetime(event)
    if current is None:
        selected[key] = _LatestEvent(event, timestamp, timestamp)
    elif timestamp is None:
        selected[key] = _LatestEvent(event, None, current.latest_valid_timestamp)
    elif current.timestamp is None:
        if (
            current.latest_valid_timestamp is None
            or timestamp > current.latest_valid_timestamp
        ):
            selected[key] = _LatestEvent(event, timestamp, timestamp)
    elif timestamp >= current.timestamp:
        selected[key] = _LatestEvent(event, timestamp, timestamp)


def _current_evidence_events(
    events: tuple[dict[str, object], ...],
    company_work: CompanyWorkRefs,
) -> tuple[tuple[dict[str, object], dict[str, object] | None], ...]:
    """Select current producer state without reconstructing its authority."""
    observed: dict[
        tuple[str, ...],
        tuple[datetime | None, datetime | None, list[dict[str, object]]],
    ] = {}
    requests: dict[str, _LatestEvent] = {}
    approvals: dict[str, dict[str, _LatestEvent]] = {}
    unkeyed_approvals: dict[str, _LatestEvent] = {}
    effects: dict[str, _LatestEvent] = {}
    producer_event_ids: dict[str, tuple[object, ...]] = {}
    work_order = company_work.work_order
    assert work_order is not None
    for event in events:
        event_type = event.get("type")
        data = event.get("data")
        if event_type == "delivery_evidence_observed":
            raw_evidence = data.get("evidence") if isinstance(data, dict) else None
            evidence_id = (
                raw_evidence.get("id") if isinstance(raw_evidence, dict) else None
            )
            producer = data.get("producer") if isinstance(data, dict) else None
            if not isinstance(raw_evidence, dict) or not isinstance(evidence_id, str):
                raise ValueError("malformed delivery_evidence_observed event")
            identity = tuple(
                str(value)
                for value in (
                    raw_evidence.get("work_order_id"),
                    raw_evidence.get("attempt_id"),
                    evidence_id,
                    raw_evidence.get("subject_kind"),
                    raw_evidence.get("subject_id"),
                    producer,
                )
            )
            observed_at: datetime | None
            try:
                parsed = DeliveryEvidenceObservedEvent.model_validate(event)
                fact = parsed.data
                observed_at = fact.observed_at
            except ValidationError:
                observed_at = _event_datetime(event)
            current_observation = observed.get(identity)
            if current_observation is None:
                observed[identity] = (observed_at, observed_at, [event])
            elif observed_at is None:
                observed[identity] = (None, current_observation[1], [event])
            elif current_observation[0] is None:
                if (
                    current_observation[1] is None
                    or observed_at > current_observation[1]
                ):
                    observed[identity] = (observed_at, observed_at, [event])
            elif observed_at > current_observation[0]:
                observed[identity] = (observed_at, observed_at, [event])
            elif observed_at == current_observation[0]:
                current_observation[2].append(event)
            continue
        if not isinstance(data, dict):
            if event_type in {
                "approval_requested",
                "approval_decided",
                "effect_state_changed",
            }:
                raise ValueError(f"malformed {event_type} event: data is not an object")
            continue
        if event_type in {
            "approval_requested",
            "approval_decided",
            "effect_state_changed",
        }:
            event_work_order = data.get("work_order_id")
            if (
                isinstance(event_work_order, str)
                and event_work_order
                and event_work_order != work_order.node_id
            ):
                continue
            event_id = data.get("event_id")
            if isinstance(event_id, str) and event_id:
                producer_identity = (event_type, event.get("source"), data)
                prior_identity = producer_event_ids.get(event_id)
                if prior_identity is not None:
                    if prior_identity != producer_identity:
                        raise ValueError(
                            f"conflicting producer event_id {event_id}"
                        )
                    continue
                producer_event_ids[event_id] = producer_identity
        if event_type == "approval_requested":
            digest = data.get("request_digest")
            if isinstance(digest, str) and digest:
                _select_latest(requests, digest, event)
            else:
                effect_id = data.get("effect_id")
                if isinstance(effect_id, str) and effect_id:
                    _select_latest(unkeyed_approvals, effect_id, event)
        elif event_type == "approval_decided":
            digest = data.get("request_digest")
            if isinstance(digest, str) and digest:
                decision = data.get("decision")
                decision_key = (
                    decision if isinstance(decision, str) and decision else "<malformed>"
                )
                _select_latest(approvals.setdefault(digest, {}), decision_key, event)
            else:
                effect_id = data.get("effect_id")
                if isinstance(effect_id, str) and effect_id:
                    _select_latest(unkeyed_approvals, effect_id, event)
        elif event_type == "effect_state_changed":
            key = data.get("idempotency_key")
            effect_id = data.get("effect_id")
            if isinstance(key, str) and key:
                _select_latest(effects, f"key:{key}", event)
            elif isinstance(effect_id, str) and effect_id:
                _select_latest(effects, f"effect:{effect_id}", event)
    selected_events: list[tuple[dict[str, object], dict[str, object] | None]] = []
    for _, _, events_for_revision in observed.values():
        for event in events_for_revision:
            data = event.get("data")
            evidence = data.get("evidence") if isinstance(data, dict) else None
            explicit_other_work = (
                isinstance(evidence, dict)
                and isinstance(evidence.get("work_order_id"), str)
                and evidence["work_order_id"] != work_order.node_id
            )
            if explicit_other_work:
                continue
            selected_events.append((event, None))
    for digest, current_request in requests.items():
        decisions = approvals.get(digest)
        if decisions:
            selected_events.extend(
                (decisions[key].event, None) for key in sorted(decisions)
            )
        else:
            selected_events.append((current_request.event, None))
    selected_events.extend(
        (decisions[key].event, None)
        for digest, decisions in approvals.items()
        if digest not in requests
        for key in sorted(decisions)
    )
    selected_events.extend(
        (current.event, None) for current in unkeyed_approvals.values()
    )
    for current in effects.values():
        event = current.event
        data = event.get("data")
        digest = data.get("request_digest") if isinstance(data, dict) else None
        matching_request = requests.get(digest) if isinstance(digest, str) else None
        approval_request = (
            matching_request.event if matching_request is not None else None
        )
        selected_events.append((event, approval_request))
    return tuple(selected_events)


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


def _undeterminable(
    diagnostic: str, *, evidence_revision: str | None = None
) -> DeliveryEvaluateResponse:
    return DeliveryEvaluateResponse(
        status="undeterminable",
        evidence_revision=evidence_revision,
        diagnostics=(diagnostic,),
    )


def _inactive_declaration() -> DeliveryEvaluateResponse:
    return DeliveryEvaluateResponse(
        status="inactive",
        diagnostics=("generic delivery requires a valid company work declaration",),
    )
