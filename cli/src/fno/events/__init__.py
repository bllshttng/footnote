"""Schema-aware event validation and typed builders for events.jsonl.

Loads the in-package ``fno/events/schema.yaml`` at module import. Failure to
load is loud: ``SchemaUnavailableError`` is raised so callers cannot silently
proceed with malformed events.

Public surface:
    validate(event: dict) -> None | raises ValidationError
    phase_transition(...) -> dict
    child_promise(...) -> dict
    mission_started(...) -> dict
    wave_advanced(...) -> dict
    mission_complete(...) -> dict

Each builder returns a fully-formed event dict that passes ``validate()``.
Builders use keyword-only arguments so unknown kwargs raise ``TypeError``
at call time without manual ``**kwargs`` handling - drift between the
schema and Python cannot ship silently.

The legacy ``fno.events.log`` and ``fno.events.cli`` modules
remain unchanged; this ``__init__`` adds the canonical envelope surface
alongside them.
"""
from __future__ import annotations

import datetime as _dt
import json as _json
import time as _time
from pathlib import Path
from typing import Any

import yaml as _yaml

from .verify_child_promise import verify_child_promise


class ValidationError(Exception):
    """Raised when an event fails schema validation."""


class SchemaUnavailableError(Exception):
    """Raised when the schema manifest cannot be loaded at module import."""


def _resolve_manifest_path(start: Path | None = None) -> Path:
    """Find the schema YAML: the sibling ``schema.yaml`` in this package.

    The schema lives AT ``fno/events/schema.yaml`` - package source in the
    dev tree and editable installs, package data in the wheel - so it is
    always beside this module with no force-include, walk-up, or env var.
    ``start`` is accepted for back-compat (tests pin a fake root) but the
    in-package sibling is authoritative.

    Raises ``SchemaUnavailableError`` if it is missing.
    """
    sibling = Path(__file__).resolve().parent / "schema.yaml"
    if sibling.is_file():
        return sibling
    raise SchemaUnavailableError(
        f"events schema not found beside the package (expected {sibling})"
    )


def _load_schema() -> dict[str, Any]:
    path = _resolve_manifest_path()
    try:
        return _yaml.safe_load(path.read_text(encoding="utf-8"))
    except _yaml.YAMLError as exc:
        raise SchemaUnavailableError(f"failed to parse {path}: {exc}") from exc


# Schema is loaded lazily so a missing manifest does NOT break module
# import. ``validate()`` and the typed builders raise SchemaUnavailableError
# when invoked without a loadable schema. Smoke-test contexts (an isolated
# venv installing the wheel) need to import fno.events without
# crashing if the YAML isn't on disk; fail at validate-time instead so
# unrelated CLI subcommands still work.
SCHEMA: dict[str, Any] | None
EVENT_TYPES: dict[str, dict[str, Any]] | None
ENVELOPE_REQUIRED: list[str]
MAX_DATA_BYTES: int
ALLOWED_SOURCES: set[str]
ALLOWED_GATES: set[str]
_schema_load_error: SchemaUnavailableError | None = None

try:
    SCHEMA = _load_schema()
    EVENT_TYPES = {e["name"]: e for e in SCHEMA.get("event_types", [])}
    ENVELOPE_REQUIRED = SCHEMA["envelope"]["required"]
    MAX_DATA_BYTES = SCHEMA.get("limits", {}).get("max_data_bytes", 65536)
    ALLOWED_SOURCES = set(SCHEMA["envelope"]["properties"]["source"]["enum"])
    ALLOWED_GATES = set(SCHEMA.get("gates", []))
except SchemaUnavailableError as _exc:
    SCHEMA = None
    EVENT_TYPES = None
    ENVELOPE_REQUIRED = []
    MAX_DATA_BYTES = 65536
    ALLOWED_SOURCES = set()
    ALLOWED_GATES = set()
    _schema_load_error = _exc


def _require_schema() -> None:
    """Raise the deferred SchemaUnavailableError if module import couldn't load."""
    if _schema_load_error is not None:
        raise _schema_load_error


def validate(event: dict[str, Any]) -> None:
    """Validate an event against the canonical envelope and per-type shape.

    Returns ``None`` on success; raises ``ValidationError`` with a single-
    line diagnostic naming the failed field. Raises
    ``SchemaUnavailableError`` if the schema YAML could not be loaded at
    module import (deferred until first validate so unrelated CLI
    subcommands can import the package).
    """
    _require_schema()
    for field in ENVELOPE_REQUIRED:
        if field not in event:
            raise ValidationError(f"event missing required field: {field}")
    if event["source"] not in ALLOWED_SOURCES:
        raise ValidationError(
            f"unknown source: {event['source']!r} "
            f"(allowed: {sorted(ALLOWED_SOURCES)})"
        )

    type_name = event["type"]
    if type_name not in EVENT_TYPES:
        raise ValidationError(f"unknown event type: {type_name}")

    type_spec = EVENT_TYPES[type_name]
    data = event.get("data") or {}

    for field in type_spec.get("data", {}).get("required", []):
        if field == "gate" and not data.get("gate_bearing", False):
            continue
        if field not in data:
            raise ValidationError(
                f"event type {type_name} missing required data field: {field}"
            )

    if type_name == "phase_transition" and data.get("gate_bearing") and not data.get("gate"):
        raise ValidationError(
            "phase_transition with gate_bearing=true must include data.gate"
        )

    if type_name == "phase_transition" and data.get("gate") and data["gate"] not in ALLOWED_GATES:
        raise ValidationError(
            f"unknown gate: {data['gate']!r} (allowed: {sorted(ALLOWED_GATES)})"
        )

    if type_name == "mission_complete":
        status = data.get("status")
        type_props = type_spec["data"]["properties"]
        allowed_statuses = type_props.get("status", {}).get("enum", [])
        if allowed_statuses and status not in allowed_statuses:
            raise ValidationError(
                f"unknown status: {status!r} (allowed: {allowed_statuses})"
            )

    # Enforce the data.source enum for session_satisfied + auto_complete_triggered
    # at validate() time so shell callers using `fno event emit --type ... --data ...`
    # (which routes through _build -> validate) can't silently land a typo. The
    # typed builders enforce the same enum at call time, but the schema-validator
    # is the chokepoint that catches all paths including the generic emit CLI.
    if type_name in ("session_satisfied", "auto_complete_triggered"):
        # Explicit indexing instead of .get(default={}) - per Gemini review on
        # PR #286: helpers validating schema-derived inputs should raise on
        # unexpected shape rather than silently degrading to "no enum check".
        # If the schema YAML lacks data.properties.source.enum for these
        # event types, that's a schema-correctness bug we want to surface.
        source_prop = type_spec["data"]["properties"]["source"]
        allowed_data_sources = source_prop["enum"]
        data_source = data.get("source")
        if data_source not in allowed_data_sources:
            raise ValidationError(
                f"unknown {type_name} data.source: {data_source!r} "
                f"(allowed: {allowed_data_sources})"
            )

    serialized = _json.dumps(data, separators=(",", ":")).encode("utf-8")
    if len(serialized) > MAX_DATA_BYTES:
        raise ValidationError(
            f"event data exceeds max_data_bytes "
            f"(got {len(serialized)}, limit {MAX_DATA_BYTES})"
        )


def _ts_now() -> str:
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _build(type_name: str, source: str, data: dict[str, Any]) -> dict[str, Any]:
    event = {"ts": _ts_now(), "type": type_name, "source": source, "data": data}
    validate(event)
    return event


def phase_transition(
    *,
    phase: str,
    nonce: str,
    session_id: str,
    source: str,
    gate: str | None = None,
    gate_bearing: bool = True,
) -> dict[str, Any]:
    """Build a ``phase_transition`` event.

    ``gate_bearing=True`` (default) requires ``gate``; the caller is
    flipping a gate in the state file and emitting the matching event.
    ``gate_bearing=False`` is for audit-only phase boundaries (the
    transition itself, not a gate flip).
    """
    data: dict[str, Any] = {
        "gate_bearing": gate_bearing,
        "phase": phase,
        "nonce": nonce,
        "session_id": session_id,
    }
    if gate is not None:
        data["gate"] = gate
    return _build("phase_transition", source, data)


def child_promise(*, session_id: str, nonce: str, source: str = "target") -> dict[str, Any]:
    """Build a ``child_promise`` event (target emits at COMPLETE; megawalk verifies)."""
    return _build("child_promise", source, {"session_id": session_id, "nonce": nonce})


def mission_started(*, mission_id: str) -> dict[str, Any]:
    """Build a ``mission_started`` event (megatron mission entered RUNNING)."""
    return _build("mission_started", "megatron", {"mission_id": mission_id})


def wave_advanced(
    *,
    mission_id: str,
    wave: int,
    child_session_ids: list[str],
) -> dict[str, Any]:
    """Build a ``wave_advanced`` event (megatron completed a wave)."""
    return _build(
        "wave_advanced",
        "megatron",
        {
            "mission_id": mission_id,
            "wave": wave,
            "child_session_ids": child_session_ids,
        },
    )


def mission_complete(*, mission_id: str, status: str) -> dict[str, Any]:
    """Build a ``mission_complete`` event (megatron reached terminal status)."""
    return _build("mission_complete", "megatron", {"mission_id": mission_id, "status": status})


PHASE_0_DECISIONS = frozenset({"abort_daemon", "reads_only_v1", "full_v1"})


def phase_0_decision(
    *,
    ratio: float,
    decision: str,
    evidence_path: str,
    source: str = "target",
) -> dict[str, Any]:
    """Build a ``phase_0_decision`` event.

    Used by the abi-daemon Phase 0 measurement spike (and any similar
    measurement-gated phases). Routes through ``_build`` so the canonical
    ``data`` envelope is used and the event passes schema validation.

    As of ab-a1118224 the ``fno event emit`` CLI subcommand also routes
    through ``_build`` + ``append_event``, so generic callers can now use
    either path. This typed builder is preferred for code paths that
    construct the event in Python (it enforces the decision enum at build
    time); the CLI is for ad-hoc / shell-level emission.

    The decision enum (``abort_daemon | reads_only_v1 | full_v1``) is
    enforced here at build time. The generic ``validate()`` checks envelope
    + source enum + presence of required data fields, but does not enforce
    per-data-field value enums beyond special cases (gate, mission status);
    enforcing the decision enum here keeps the builder honest without
    expanding the validator's scope.
    """
    if decision not in PHASE_0_DECISIONS:
        raise ValidationError(
            f"unknown phase_0 decision: {decision!r} "
            f"(allowed: {sorted(PHASE_0_DECISIONS)})"
        )
    return _build(
        "phase_0_decision",
        source,
        {"ratio": ratio, "decision": decision, "evidence_path": evidence_path},
    )


INTEGRITY_WARNING_KINDS = frozenset({"missing_nonce_legacy_accepted"})


def integrity_warning(
    *,
    kind: str,
    phase: str,
    session_id: str,
    artifact_path: str,
    source: str = "hook",
) -> dict[str, Any]:
    """Build an ``integrity_warning`` event.

    Forensic notice that a gate verification path accepted a degraded input.
    The ``kind`` enum is enforced here at build time. The generic
    ``validate()`` checks envelope + source enum + presence of required data
    fields, but does not enforce per-data-field value enums beyond a few
    special cases; enforcing the kind enum here keeps the builder honest.
    """
    if kind not in INTEGRITY_WARNING_KINDS:
        raise ValidationError(
            f"unknown integrity_warning kind: {kind!r} "
            f"(allowed: {sorted(INTEGRITY_WARNING_KINDS)})"
        )
    return _build(
        "integrity_warning",
        source,
        {
            "kind": kind,
            "phase": phase,
            "session_id": session_id,
            "artifact_path": artifact_path,
        },
    )


def done_race_collision(
    *,
    node_id: str,
    first_completed_at: str,
    second_attempt_at: str,
    source: str = "abi-loop",
) -> dict[str, Any]:
    """Build a ``done_race_collision`` event.

    Forensic notice that two ``fno done`` calls landed on the same node; the
    second saw ``_status`` already done. Emitted AFTER ``locked_mutate_graph``
    returns so the event reflects the actual outcome of the metadata writes.
    """
    return _build(
        "done_race_collision",
        source,
        {
            "node_id": node_id,
            "first_completed_at": first_completed_at,
            "second_attempt_at": second_attempt_at,
        },
    )


def backlog_done_refused(
    *,
    node_id: str,
    pr_number: int,
    reason: str,
    source: str = "backlog",
) -> dict[str, Any]:
    """Build a ``backlog_done_refused`` event.

    Emitted when ``fno backlog done`` refuses to close a node because no
    merged/green-CI evidence was found for any referenced PR.
    """
    return _build(
        "backlog_done_refused",
        source,
        {
            "node_id": node_id,
            "pr_number": pr_number,
            "reason": reason,
        },
    )


def backlog_done_forced(
    *,
    node_id: str,
    force_reason: str,
    pr_number: int | None = None,
    pr_state: str | None = None,
    source: str = "backlog",
) -> dict[str, Any]:
    """Build a ``backlog_done_forced`` event.

    Emitted when ``fno backlog done --force --reason TEXT`` closes a node,
    bypassing the gh cross-check. Carries the operator-supplied reason and
    the gh evidence that was present at close time (if any).
    """
    data: dict[str, Any] = {
        "node_id": node_id,
        "force_reason": force_reason,
    }
    if pr_number is not None:
        data["pr_number"] = pr_number
    if pr_state is not None:
        data["pr_state"] = pr_state
    return _build("backlog_done_forced", source, data)


SESSION_SATISFIED_SOURCES = frozenset(
    {"check_pr", "pr_merge", "ci_watcher", "abi_gate_manual", "delegated"}
)
# "delegated" is shell-emitted (skills/target/scripts/handoff.sh); the rest are Python-emitted.


def session_satisfied(
    *,
    trigger: str,
    reason: str,
    session_id: str,
    gate_state_hash: str,
    evidence_url: str | None = None,
    source: str = "target",
) -> dict[str, Any]:
    """Build a ``session_satisfied`` event.

    Alternative to <promise> tag emission. The target stop hook scans for
    these and may auto-release when a fresh event matches the current
    session and the three-factor gates are still satisfied.

    ``trigger`` is the constrained data-level enum identifying which
    subsystem produced the signal. ``source`` is the envelope-level
    producer identity (target, megawalk, abi-loop, hook).

    The enum is enforced here at build time so a typo at the call site
    fails fast rather than landing in events.jsonl as schema noise.
    """
    if trigger not in SESSION_SATISFIED_SOURCES:
        raise ValidationError(
            f"unknown session_satisfied trigger: {trigger!r} "
            f"(allowed: {sorted(SESSION_SATISFIED_SOURCES)})"
        )
    # Non-empty guards on the audit-load-bearing fields. The CLI surface
    # also guards reason but a programmatic caller can reach the builder
    # directly; an empty string passing schema validation defeats the
    # audit-trail purpose of these fields.
    if not reason or not reason.strip():
        raise ValidationError("session_satisfied reason cannot be empty")
    if not session_id or not session_id.strip():
        raise ValidationError("session_satisfied session_id cannot be empty")
    if not gate_state_hash or not gate_state_hash.strip():
        raise ValidationError("session_satisfied gate_state_hash cannot be empty")
    data: dict[str, Any] = {
        "source": trigger,
        "reason": reason,
        "session_id": session_id,
        "gate_state_hash": gate_state_hash,
    }
    if evidence_url is not None:
        data["evidence_url"] = evidence_url
    return _build("session_satisfied", source, data)


def auto_complete_triggered(
    *,
    trigger: str,
    session_id: str,
    source: str = "hook",
) -> dict[str, Any]:
    """Build an ``auto_complete_triggered`` event.

    Audit-only emission written by the stop hook after it fires the
    auto-complete path. ``trigger`` mirrors the data.source of the
    session_satisfied event that activated this completion.
    """
    if trigger not in SESSION_SATISFIED_SOURCES:
        raise ValidationError(
            f"unknown auto_complete_triggered trigger: {trigger!r} "
            f"(allowed: {sorted(SESSION_SATISFIED_SOURCES)})"
        )
    if not session_id or not session_id.strip():
        raise ValidationError("auto_complete_triggered session_id cannot be empty")
    return _build(
        "auto_complete_triggered",
        source,
        {"source": trigger, "session_id": session_id},
    )


def append_event(
    event: dict[str, Any],
    events_path: Path | None = None,
    *,
    lock_timeout_seconds: int = 30,
) -> None:
    """Append a validated event to events.jsonl under a mkdir mutex.

    Validates the event before acquiring the lock so a malformed payload
    cannot block other writers. The mutex directory matches the convention
    used by ``scripts/lib/set-gate.sh`` and ``scripts/migrate-events-shape.py``,
    so cross-language callers serialize correctly on the same path.
    """
    validate(event)

    if events_path is None:
        events_path = Path(".fno/events.jsonl")
    events_path.parent.mkdir(parents=True, exist_ok=True)

    lock_dir = events_path.parent / (events_path.name + ".lock.d")
    deadline = _time.monotonic() + lock_timeout_seconds
    while True:
        try:
            lock_dir.mkdir()
            break
        except FileExistsError:
            if _time.monotonic() >= deadline:
                raise TimeoutError(f"events.jsonl lock timeout: {lock_dir}")
            _time.sleep(0.1)

    try:
        with events_path.open("a", encoding="utf-8") as fh:
            fh.write(_json.dumps(event, separators=(",", ":")) + "\n")
    finally:
        try:
            lock_dir.rmdir()
        except OSError:
            pass


__all__ = [
    "ALLOWED_GATES",
    "ALLOWED_SOURCES",
    "ENVELOPE_REQUIRED",
    "EVENT_TYPES",
    "MAX_DATA_BYTES",
    "SCHEMA",
    "SESSION_SATISFIED_SOURCES",
    "SchemaUnavailableError",
    "ValidationError",
    "INTEGRITY_WARNING_KINDS",
    "append_event",
    "auto_complete_triggered",
    "backlog_done_forced",
    "backlog_done_refused",
    "child_promise",
    "done_race_collision",
    "integrity_warning",
    "mission_complete",
    "mission_started",
    "phase_0_decision",
    "phase_transition",
    "session_satisfied",
    "validate",
    "verify_child_promise",
    "wave_advanced",
]
