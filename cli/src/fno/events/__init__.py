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
import hashlib as _hashlib
import json as _json
import os
import re as _re
import secrets as _secrets
from threading import RLock as _RLock
from pathlib import Path
from typing import TYPE_CHECKING, Any, cast

import yaml as _yaml

from ..config._dispatch_verbs import is_verb_seed
from ..paths import EPHEMERAL_EVENTS_SUFFIX as EPHEMERAL_SUFFIX
from .store_client import EventStoreUnavailable, resolve_native_bin
from .verify_child_promise import FanInTally, tally_fan_in, verify_child_promise


class ValidationError(Exception):
    """Raised when an event fails schema validation."""


class SchemaUnavailableError(Exception):
    """Raised when the schema manifest cannot be loaded at module import."""






def _resolve_manifest_path() -> Path:
    """Find the schema YAML: the sibling ``schema.yaml`` in this package.

    The schema lives AT ``fno/events/schema.yaml`` - package source in the
    dev tree and editable installs, package data in the wheel - so it is
    always beside this module with no force-include, walk-up, or env var.

    Raises ``SchemaUnavailableError`` if it is missing.
    """
    sibling = Path(__file__).resolve().parent / "schema.yaml"
    if sibling.is_file():
        return sibling
    raise SchemaUnavailableError(f"events schema not found beside the package (expected {sibling})")


def _load_schema() -> dict[str, Any]:
    path = _resolve_manifest_path()
    try:
        return _yaml.safe_load(path.read_text(encoding="utf-8"))
    except _yaml.YAMLError as exc:
        raise SchemaUnavailableError(f"failed to parse {path}: {exc}") from exc


_RETENTION_CLASSES = frozenset({"ephemeral", "gate", "telemetry", "durable"})


def validate_retention_schema(schema: dict[str, Any]) -> None:
    """Validate retention classes and every declared join as one contract."""
    retention = schema.get("retention", {})
    default = retention.get("default", "durable")
    if default not in _RETENTION_CLASSES:
        raise SchemaUnavailableError(f"invalid retention default: {default!r}")
    minimum = retention.get("minimum_ephemeral_ttl_hours", 672)
    if not isinstance(minimum, int) or isinstance(minimum, bool) or minimum <= 0:
        raise SchemaUnavailableError(f"invalid minimum_ephemeral_ttl_hours: {minimum!r}")
    entries = {entry.get("name"): entry for entry in schema.get("event_types", [])}
    for name, entry in entries.items():
        value = entry.get("retention", default)
        if value not in _RETENTION_CLASSES:
            raise SchemaUnavailableError(f"invalid retention class for {name}: {value!r}")
    for pair in retention.get("joins", []):
        if not isinstance(pair, list) or len(pair) != 2:
            raise SchemaUnavailableError(f"invalid retention join: {pair!r}")
        left, right = pair
        if left not in entries or right not in entries:
            raise SchemaUnavailableError(f"retention join names unknown event type: {pair!r}")
        left_class = entries[left].get("retention", default)
        right_class = entries[right].get("retention", default)
        if left_class != right_class:
            raise SchemaUnavailableError(
                f"retention join mismatch: {left}={left_class}, {right}={right_class}"
            )


# Schema is loaded lazily so importing fno.events does not parse the manifest.
# ``validate()`` and the typed builders raise SchemaUnavailableError when
# invoked without a loadable schema. Smoke-test contexts (an isolated venv
# installing the wheel) need to import fno.events without crashing if the YAML
# isn't on disk; fail at validate-time instead so unrelated CLI subcommands
# still work.
SCHEMA: dict[str, Any] | None
EVENT_TYPES: dict[str, dict[str, Any]] | None
ENVELOPE_REQUIRED: list[str]
MAX_DATA_BYTES: int
DATA_SIZE_ENCODING: str
ALLOWED_SOURCES: set[str]
# per-agent worker sources (worker:<id>, stream-worker:<id>) validate by
# regex, not enum membership. Compiled from envelope.properties.source.patterns.
ALLOWED_SOURCE_PATTERNS: list[Any]
ALLOWED_GATES: set[str]
RETENTION_DEFAULT: str
RETENTION_MINIMUM_TTL_HOURS: int
# EPHEMERAL_SUFFIX (sibling journal for ephemeral-class rows) is
# aliased from fno.paths at import time, one definition shared by every
# Python reader and writer; a parity test holds it equal to the Rust const.
_schema_load_error: SchemaUnavailableError | None = None

if TYPE_CHECKING:
    # These names are populated dynamically after import by
    # _ensure_schema_loaded(). Keep them visible to static analysis without
    # creating module attributes that would defeat __getattr__.
    SCHEMA = cast(dict[str, Any] | None, None)
    EVENT_TYPES = cast(dict[str, dict[str, Any]] | None, None)
    ENVELOPE_REQUIRED = cast(list[str], None)
    MAX_DATA_BYTES = cast(int, None)
    DATA_SIZE_ENCODING = cast(str, None)
    ALLOWED_SOURCES = cast(set[str], None)
    ALLOWED_SOURCE_PATTERNS = cast(list[Any], None)
    ALLOWED_GATES = cast(set[str], None)
    RETENTION_DEFAULT = cast(str, None)
    RETENTION_MINIMUM_TTL_HOURS = cast(int, None)
    PROTOCOL_FAMILY_TYPES = cast(set[str], None)
    PROTOCOL_FAMILY_VERSION = cast(int, None)
    PROTOCOL_ENVELOPE_ALLOWED = cast(set[str], None)
    PROTOCOL_ENVELOPE_REQUIRED = cast(list[str], None)
    PROTOCOL_OUTCOME_ENUM = cast(set[str], None)
    PROTOCOL_OUTCOME_ON = cast(set[str], None)
    validate = cast(Any, None)  # served through __getattr__; the native shim

_schema_loaded = False
_schema_lock = _RLock()
_SCHEMA_PUBLIC_NAMES = frozenset(
    {
        "SCHEMA",
        "EVENT_TYPES",
        "ENVELOPE_REQUIRED",
        "MAX_DATA_BYTES",
        "DATA_SIZE_ENCODING",
        "ALLOWED_SOURCES",
        "ALLOWED_SOURCE_PATTERNS",
        "ALLOWED_GATES",
        "RETENTION_DEFAULT",
        "RETENTION_MINIMUM_TTL_HOURS",
        "PROTOCOL_FAMILY_TYPES",
        "PROTOCOL_FAMILY_VERSION",
        "PROTOCOL_ENVELOPE_ALLOWED",
        "PROTOCOL_ENVELOPE_REQUIRED",
        "PROTOCOL_OUTCOME_ENUM",
        "PROTOCOL_OUTCOME_ON",
    }
)


def _ensure_schema_loaded() -> None:
    """Populate schema-derived globals on their first real use."""
    global _schema_loaded, _schema_load_error
    if _schema_loaded:
        return
    with _schema_lock:
        if _schema_loaded:
            return
        try:
            schema = _load_schema()
            validate_retention_schema(schema)
            globals().update(
                SCHEMA=schema,
                EVENT_TYPES={e["name"]: e for e in schema.get("event_types", [])},
                ENVELOPE_REQUIRED=schema["envelope"]["required"],
                MAX_DATA_BYTES=schema.get("limits", {}).get("max_data_bytes", 65536),
                DATA_SIZE_ENCODING=schema.get("limits", {}).get("data_size_encoding", ""),
                ALLOWED_SOURCES=set(schema["envelope"]["properties"]["source"]["enum"]),
                ALLOWED_SOURCE_PATTERNS=[
                    _re.compile(p)
                    for p in schema["envelope"]["properties"]["source"].get("patterns", [])
                ],
                ALLOWED_GATES=set(schema.get("gates", [])),
                RETENTION_DEFAULT=schema.get("retention", {}).get("default", "durable"),
                RETENTION_MINIMUM_TTL_HOURS=int(
                    schema.get("retention", {}).get("minimum_ephemeral_ttl_hours", 672)
                ),
            )
            if DATA_SIZE_ENCODING != "compact-json-ascii-v1":
                raise SchemaUnavailableError(
                    f"unsupported limits.data_size_encoding: {DATA_SIZE_ENCODING!r}"
                )
            # a2a status-breakpoint family: types carrying the extended envelope.
            family = schema.get("protocol_family", {})
            globals().update(
                PROTOCOL_FAMILY_TYPES=set(family.get("types", [])),
                PROTOCOL_FAMILY_VERSION=family.get("version", 1),
                PROTOCOL_ENVELOPE_ALLOWED=set(family.get("envelope", {}).get("allowed", [])),
                PROTOCOL_ENVELOPE_REQUIRED=list(family.get("envelope", {}).get("required", [])),
                PROTOCOL_OUTCOME_ENUM=set(family.get("outcome", {}).get("enum", [])),
                PROTOCOL_OUTCOME_ON=set(family.get("outcome", {}).get("present_on", [])),
            )
        except SchemaUnavailableError as _exc:
            globals().update(
                SCHEMA=None,
                EVENT_TYPES=None,
                ENVELOPE_REQUIRED=[],
                MAX_DATA_BYTES=65536,
                DATA_SIZE_ENCODING="",
                ALLOWED_SOURCES=set(),
                ALLOWED_SOURCE_PATTERNS=[],
                ALLOWED_GATES=set(),
                RETENTION_DEFAULT="durable",
                RETENTION_MINIMUM_TTL_HOURS=672,
                PROTOCOL_FAMILY_TYPES=set(),
                PROTOCOL_FAMILY_VERSION=1,
                PROTOCOL_ENVELOPE_ALLOWED=set(),
                PROTOCOL_ENVELOPE_REQUIRED=[],
                PROTOCOL_OUTCOME_ENUM=set(),
                PROTOCOL_OUTCOME_ON=set(),
            )
            _schema_load_error = _exc
        _schema_loaded = True


def __getattr__(name: str) -> Any:
    """Load schema-derived compatibility exports only when accessed."""
    if name == "validate":
        # The judge itself lives in the native store; the shim is served
        # lazily so the module carries no ``validate`` definition of its own.
        return _validate_via_store
    if name in _SCHEMA_PUBLIC_NAMES:
        _ensure_schema_loaded()
        return globals()[name]
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


MAX_SAFE_EVENT_INTEGER = 9_007_199_254_740_991


def _require_schema() -> None:
    """Raise the deferred SchemaUnavailableError if module import couldn't load."""
    _ensure_schema_loaded()
    if _schema_load_error is not None:
        raise _schema_load_error


def retention_for(event_type: str) -> str:
    """Return an event type's retention class, defaulting unknowns to durable."""
    _require_schema()
    entry = EVENT_TYPES.get(event_type) if EVENT_TYPES is not None else None
    return entry.get("retention", RETENTION_DEFAULT) if entry else RETENTION_DEFAULT


def _utc_timestamp(value: Any) -> _dt.datetime | None:
    """Parse an RFC3339 UTC timestamp; status_fanout reads it beside the judge."""
    if not isinstance(value, str) or not value:
        return None
    m = _re.fullmatch(
        r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.([0-9]{1,9}))?(?:Z|\+00:00)",
        value,
    )
    if m is None:
        return None
    frac = m.group(1)
    if frac is not None and len(frac) > 6:
        # Nanosecond fractions (Rust emitters) floor to microseconds; every
        # consumer keys on the same truncated instant.
        value = f"{value[: value.index('.')]}.{frac[:6]}Z"
    try:
        parsed = _dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None or parsed.utcoffset() != _dt.timedelta(0):
        return None
    return parsed


def _validate_via_store(event: dict[str, Any]) -> None:
    """Judge through the native door: the store's judge is the one owner.

    Kept only until the reader families (pr, scoreboard, worktree_cli)
    port their read-side re-validation in their own children; the dual
    inventory row names it. Served as the module attribute ``validate``
    through ``__getattr__`` so the judge symbol itself stays deleted.
    """
    import subprocess

    line = _json.dumps(event, separators=(",", ":"), ensure_ascii=False)
    proc = subprocess.run(
        [resolve_native_bin(), "doctor", "event", "emit-envelope", "--validate-only"],
        input=line,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if proc.returncode == 0:
        return
    if proc.returncode == 1:
        raise ValidationError(proc.stderr.strip())
    raise EventStoreUnavailable(
        f"event validator substrate failure: {proc.stderr.strip()}"
    )


def _ts_now() -> str:
    return (
        _dt.datetime.now(_dt.timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z")
    )


def _build(
    type_name: str,
    source: str,
    data: dict[str, Any],
    envelope: dict[str, Any] | None = None,
) -> dict[str, Any]:
    event = {"ts": _ts_now(), "type": type_name, "source": source, "data": data}
    if envelope:
        # Extra top-level routable fields (protocol family). A None value
        # means "omit" (a non-session producer drops from/model entirely rather
        # than faking an empty string), so it is never written.
        for k, v in envelope.items():
            if v is not None:
                event[k] = v
    _validate_via_store(event)
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


def context_snapshot(
    *,
    session_id: str,
    harness: str,
    entry_state: str,
    context_bytes: int,
    estimated_tokens: int,
    context_hash: str | None,
    source_hashes: list[str],
    source_manifest: list[dict[str, Any]],
    measurement_complete: bool,
    measurement_errors: list[str] | None = None,
    node_id: str | None = None,
    source: str = "hook",
) -> dict[str, Any]:
    """Build one exact runtime context-delivery observation."""
    if not session_id.strip():
        raise ValidationError("context_snapshot session_id cannot be empty")
    if harness not in {"claude", "codex", "gemini"}:
        raise ValidationError(f"unknown context_snapshot harness: {harness!r}")
    if entry_state not in {"startup", "resume", "clear", "post_compact"}:
        raise ValidationError(f"unknown context_snapshot entry_state: {entry_state!r}")
    if context_bytes < 0 or estimated_tokens < 0:
        raise ValidationError("context_snapshot byte and token counts cannot be negative")
    data: dict[str, Any] = {
        "session_id": session_id,
        "harness": harness,
        "entry_state": entry_state,
        "context_bytes": context_bytes,
        "estimated_tokens": estimated_tokens,
        "context_hash": context_hash,
        "source_hashes": source_hashes,
        "source_manifest": source_manifest,
        "measurement_complete": measurement_complete,
        "measurement_errors": measurement_errors or [],
    }
    if node_id:
        data["node_id"] = node_id
    return _build("context_snapshot", source, data)


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

    Used by the fno-daemon Phase 0 measurement spike (and any similar
    measurement-gated phases). Routes through ``_build`` so the canonical
    ``data`` envelope is used and the event passes schema validation.

    As of the ``fno doctor event emit`` CLI subcommand also routes
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
            f"unknown phase_0 decision: {decision!r} (allowed: {sorted(PHASE_0_DECISIONS)})"
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
            f"unknown integrity_warning kind: {kind!r} (allowed: {sorted(INTEGRITY_WARNING_KINDS)})"
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


MAIL_ESCALATION_REASONS = frozenset({"question", "attended-miss", "reachable-miss"})


def mail_escalation(
    *,
    reason: str,
    sender: str,
    recipient: str,
    summary: str,
    msg_id: str | None = None,
    source: str = "target",
) -> dict[str, Any]:
    """Build a ``mail_escalation`` event (mail that needs a human now).

    Emitted from the shared escalation helper for two reasons: a ``question``
    send (unconditional) or a live-miss to an operator-attended recipient. The
    overlay renders it squadless-live, so the nudge survives even when the OS
    notifier was missed or unavailable. The ``reason`` enum is enforced here at
    build time (same chokepoint rationale as the other enum-bearing events); the
    generic ``fno doctor event emit`` CLI reaches ``validate()`` too, where the same
    enum is checked so a shell typo cannot land.
    """
    if reason not in MAIL_ESCALATION_REASONS:
        raise ValidationError(
            f"unknown mail_escalation reason: {reason!r} "
            f"(allowed: {sorted(MAIL_ESCALATION_REASONS)})"
        )
    data: dict[str, Any] = {
        "reason": reason,
        "sender": sender,
        "recipient": recipient,
        "summary": summary,
    }
    if msg_id is not None:
        data["msg_id"] = msg_id
    return _build("mail_escalation", source, data)


QUESTION_CAP = 2000


def operator_question(
    *,
    question_id: str,
    question: str,
    session_id: str | None = None,
    cwd: str | None = None,
    node: str | None = None,
    asker: str | None = None,
    ask: str | None = None,
    options: "list[str] | None" = None,
    blocks: "list[str] | None" = None,
    subject: str | None = None,
    source: str = "target",
) -> dict[str, Any]:
    """Build an ``operator_question`` event (an agent needs a human answer).

    Captured at ASK time. Deriving it later from transcript shape loses it the
    moment another turn lands, which is the common case in a mail-driven mesh.
    """
    data: dict[str, Any] = {
        "question_id": question_id,
        "question": question[:QUESTION_CAP],
    }
    for key, value in (
        ("session_id", session_id),
        ("cwd", cwd),
        ("node", node),
        ("asker", asker),
        ("ask", ask),
        ("options", options),
        ("blocks", blocks),
        ("subject", subject),
    ):
        if value is not None:
            data[key] = value
    return _build("operator_question", source, data)


def operator_question_closed(
    *,
    question_id: str,
    answer: str | None = None,
    closed_by: str | None = None,
    source: str = "target",
) -> dict[str, Any]:
    """Build an ``operator_question_closed`` event.

    Explicit close only - no auto-expiry, and the fold treats a double close as
    a no-op rather than an error.
    """
    data: dict[str, Any] = {"question_id": question_id}
    if answer is not None:
        data["answer"] = answer[:QUESTION_CAP]
    if closed_by is not None:
        data["closed_by"] = closed_by
    return _build("operator_question_closed", source, data)


NOTICE_CAP = 2000


def operator_notice(
    *,
    title: str,
    body: str,
    pointer: str = "",
    source: str = "python",
) -> dict[str, Any]:
    """Build an ``operator_notice`` event (the notify chokepoint's journal leg).

    A notice is a pointer to the durable queue, never a second inbox, so
    ``pointer`` carries the verb that shows the content and ``body`` carries
    counts, not rows.
    """
    data: dict[str, Any] = {
        "title": title[:NOTICE_CAP],
        "body": body[:NOTICE_CAP],
    }
    if pointer:
        data["pointer"] = pointer[:NOTICE_CAP]
    return _build("operator_notice", source, data)


def operator_decision(
    *,
    decision_id: str,
    decision: str,
    subject: str | None = None,
    question_id: str | None = None,
    question: str | None = None,
    asked_by: str | None = None,
    asked_at: str | None = None,
    expiry_ref: dict[str, Any] | None = None,
    options: "list[str] | None" = None,
    decided_by: str | None = None,
    attested_by: str | None = None,
    relayed_by: str | None = None,
    origin: str | None = None,
    authority_source: str | None = None,
    graduation: dict[str, str] | None = None,
    rationale: str | None = None,
    supersedes: str | None = None,
    reads: "list[dict[str, Any]] | None" = None,
    scope: str | None = None,
    source: str = "target",
) -> dict[str, Any]:
    """Build an ``operator_decision`` event (a durable decision record).

    Modeled on ``approval_decided``: who decided, under what authority, and
    what was on the table. ``subject`` is the recovery key a later agent
    queries; ``supersedes`` orders two decisions on one subject so a reader
    of the older one can tell it is not current. ``reads`` carries the
    executed evidence rows for a code fact the ruling asserts (see
    the evidence gate): cmd, exit code, output head.
    """
    data: dict[str, Any] = {"decision_id": decision_id, "decision": decision[:QUESTION_CAP]}
    for key, value in (
        ("subject", subject),
        ("question_id", question_id),
        ("question", question[:QUESTION_CAP] if question else None),
        ("asked_by", asked_by),
        ("asked_at", asked_at),
        ("expiry_ref", expiry_ref),
        ("options", options),
        ("decided_by", decided_by),
        ("attested_by", attested_by),
        ("relayed_by", relayed_by),
        ("origin", origin),
        ("authority_source", authority_source),
        ("graduation", graduation),
        ("rationale", rationale[:QUESTION_CAP] if rationale else None),
        ("supersedes", supersedes),
        ("reads", reads),
        ("scope", scope),
    ):
        if value is not None:
            data[key] = value
    return _build("operator_decision", source, data)


def decision_retracted(
    *,
    retraction_id: str | None = None,
    target_decision_id: str,
    subject: str,
    reason: str,
    retracted_by: str | None = None,
    attested_by: str | None = None,
    relayed_by: str | None = None,
    origin: str | None = None,
    authority_source: str | None = None,
    source: str = "target",
) -> dict[str, Any]:
    """Build an append-only event that retracts one decision."""
    data: dict[str, Any] = {
        "retraction_id": retraction_id or f"r-{_secrets.token_hex(4)}",
        "target_decision_id": target_decision_id,
        "subject": subject,
        "reason": reason[:QUESTION_CAP],
    }
    for key, value in (
        ("retracted_by", retracted_by),
        ("attested_by", attested_by),
        ("relayed_by", relayed_by),
        ("origin", origin),
        ("authority_source", authority_source),
    ):
        if value is not None:
            data[key] = value
    return _build("decision_retracted", source, data)


def agent_raw_inject(
    *,
    target_session: str,
    payload: str,
    harness: str,
    lane: str,
    sender: str | None = None,
    target_cwd: str | None = None,
    target_head: str | None = None,
    confirmed: bool | None = None,
    origin: str | None = None,
    verb: str | None = None,
    self_send: bool = False,
    source: str = "daemon",
) -> dict[str, Any]:
    """Build an ``agent_raw_inject`` provenance event.

    Records an UNWRAPPED injection (no ``<fno_mail`` envelope) at the transport,
    so the audit trail survives the loss of the in-transcript marker (
    greppability property moves to the ledger). Keyed on the payload not starting
    with ``<fno_mail``; emitted best-effort from both the mail-inject binary and
    the mux pane send. ``sender``/``target_cwd``/``target_head`` are optional
    enrichments the transport populates when the invoking layer knows them.
    ``confirmed`` is the transport's own answer, so the record never asserts an
    injection a stalled pane or an absent daemon did not perform; it is emitted
    after the send, and its ``False`` covers both a clean refusal and a landed
    payload the confirm budget missed.
    """
    if verb is None and is_verb_seed(payload):
        verb = payload.split(maxsplit=1)[0]
    if not isinstance(self_send, bool):
        raise ValidationError("agent_raw_inject self_send must be a boolean")
    data: dict[str, Any] = {
        "target_session": target_session,
        "payload": payload,
        "harness": harness,
        "lane": lane,
        "verb": verb,
        "self_send": self_send,
    }
    if sender is not None:
        data["sender"] = sender
    if target_cwd is not None:
        data["target_cwd"] = target_cwd
    if target_head is not None:
        data["target_head"] = target_head
    if confirmed is not None:
        data["confirmed"] = confirmed
    if origin is not None:
        data["origin"] = origin
    return _build("agent_raw_inject", source, data)


def mail_origin_classified(
    *,
    origin: str,
    lane: str,
    presumed_human: bool,
    sender: str | None = None,
    target_session: str | None = None,
    source: str = "daemon",
) -> dict[str, Any]:
    """Build the positive mail-origin measurement emitted before delivery.

    ``presumed_human`` records the measured positive case rather than treating
    a missing non-human row as proof that classification ran.
    """
    data: dict[str, Any] = {
        "origin": origin,
        "lane": lane,
        "presumed_human": presumed_human,
    }
    if sender is not None:
        data["sender"] = sender
    if target_session is not None:
        data["target_session"] = target_session
    return _build("mail_origin_classified", source, data)


def done_race_collision(
    *,
    node_id: str,
    first_completed_at: str,
    second_attempt_at: str,
    source: str = "fno-loop",
) -> dict[str, Any]:
    """Build a ``done_race_collision`` event.

    Forensic notice that two ``backlog done`` calls landed on the same node; the
    second saw ``status`` already done. Emitted AFTER ``locked_mutate_graph``
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


def backlog_reopened(
    *,
    node_id: str,
    reason: str,
    forced: bool = False,
    pr_number: int | None = None,
    pr_state: str | None = None,
    cascade_reopened: "list[str] | None" = None,
    source: str = "backlog",
) -> dict[str, Any]:
    """Build a ``backlog_reopened`` event.

    Emitted when ``fno backlog reopen`` clears a node's completion. The reason
    is required rather than optional: closing a node is evidenced by a merged
    PR, and reopening one is evidenced by nothing but the operator's judgment,
    so the judgment is the record.

    ``cascade_reopened`` carries the ancestor epics reopened alongside, which
    are the ones ``_cascade_close_parents`` had auto-closed when this node
    closed. Joined into a string because the envelope's data values are scalars.
    """
    data: dict[str, Any] = {"node_id": node_id, "reason": reason, "forced": forced}
    if pr_number is not None:
        data["pr_number"] = pr_number
    if pr_state is not None:
        data["pr_state"] = pr_state
    if cascade_reopened:
        data["cascade_reopened"] = ",".join(cascade_reopened)
    return _build("backlog_reopened", source, data)


SESSION_SATISFIED_SOURCES = frozenset(
    {"check_pr", "pr_merge", "ci_watcher", "fno_gate_manual", "delegated"}
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
    producer identity (target, megawalk, fno-loop, hook).

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


def _overlap_observation_id(
    repository_key: str,
    worktree_key: str,
    observer_session_id: str,
    peer_session_ids: list[str],
) -> str:
    """Deterministic digest of the (repo, worktree, observer, sorted peers) tuple.

    JSON-encode the tuple so a field containing the delimiter (or any byte)
    cannot make two different tuples collide on the same bytes. The peer list is
    already sorted+deduped by the caller, so repeated deliveries of one
    observation yield one id."""
    blob = _json.dumps(
        [repository_key, worktree_key, observer_session_id, peer_session_ids],
        separators=(",", ":"),
        ensure_ascii=False,
    )
    return _hashlib.sha256(blob.encode("utf-8")).hexdigest()


def worktree_overlap_observed(
    *,
    observer_session_id: str,
    peer_session_ids: list[str],
    repository_key: str,
    worktree_key: str,
    live_window_seconds: int = 120,
    harness: str | None = None,
    source: str = "hook",
) -> dict[str, Any]:
    """Build a ``worktree_overlap_observed`` event.

    Advisory-only telemetry: one structured record of a SessionStart peer
    observation, written to the machine-global journal so it survives worktree
    cleanup. ``observation_id`` is a deterministic digest of the repository,
    worktree, observer, and sorted peer set, so repeated deliveries of the
    same observation dedupe in the recurrence fold. ``harness`` is diagnostic
    metadata only; it never shapes the predicate, the observation id, or the
    recurrence threshold.
    """
    if not isinstance(observer_session_id, str) or not observer_session_id:
        raise ValidationError("worktree_overlap_observed observer_session_id cannot be empty")
    if (
        not isinstance(peer_session_ids, list)
        or not peer_session_ids
        or not all(isinstance(p, str) and p for p in peer_session_ids)
    ):
        raise ValidationError(
            "worktree_overlap_observed peer_session_ids must be a non-empty "
            "list of non-empty strings"
        )
    if not isinstance(repository_key, str) or not repository_key:
        raise ValidationError("worktree_overlap_observed repository_key cannot be empty")
    if not isinstance(worktree_key, str) or not worktree_key:
        raise ValidationError("worktree_overlap_observed worktree_key cannot be empty")
    if (
        not isinstance(live_window_seconds, int)
        or isinstance(live_window_seconds, bool)
        or live_window_seconds <= 0
    ):
        raise ValidationError(
            "worktree_overlap_observed live_window_seconds must be a positive integer"
        )
    sorted_peers = sorted(set(peer_session_ids))
    data: dict[str, Any] = {
        "observation_id": _overlap_observation_id(
            repository_key, worktree_key, observer_session_id, sorted_peers
        ),
        "repository_key": repository_key,
        "worktree_key": worktree_key,
        "observer_session_id": observer_session_id,
        "peer_session_ids": sorted_peers,
        "live_window_seconds": live_window_seconds,
    }
    if harness is not None:
        data["harness"] = harness
    return _build("worktree_overlap_observed", source, data)


_MISSING = object()


def config_write(
    *,
    key: str,
    scope: str,
    root_kind: str,
    config_path: str,
    present_before: bool,
    present_after: bool,
    old_value: Any = _MISSING,
    new_value: Any = _MISSING,
    redacted: bool = False,
    attester_session_id: str,
    attester_witness: str,
) -> dict[str, Any]:
    """Build a typed receipt for one changed config leaf."""
    if not isinstance(key, str) or not key.strip():
        raise ValidationError("config_write key cannot be empty")
    if scope not in {"global", "project"}:
        raise ValidationError(f"unknown config_write scope: {scope!r}")
    if root_kind not in {"operator", "project"}:
        raise ValidationError(f"unknown config_write root_kind: {root_kind!r}")
    if not isinstance(config_path, str) or not config_path:
        raise ValidationError("config_write config_path cannot be empty")
    if not isinstance(present_before, bool) or not isinstance(present_after, bool):
        raise ValidationError("config_write presence flags must be booleans")
    if not present_before and not present_after:
        raise ValidationError("config_write cannot record a no-change write")
    if present_before and old_value is _MISSING:
        raise ValidationError("config_write old_value is required when present_before is true")
    if present_after and new_value is _MISSING:
        raise ValidationError("config_write new_value is required when present_after is true")
    if not isinstance(redacted, bool):
        raise ValidationError("config_write redacted must be a boolean")
    if not isinstance(attester_session_id, str):
        raise ValidationError("config_write attester_session_id must be a string")
    if attester_witness not in {"process", "env_only", "conflict"}:
        raise ValidationError(f"unknown config_write attester_witness: {attester_witness!r}")

    data: dict[str, Any] = {
        "key": key,
        "scope": scope,
        "root_kind": root_kind,
        "config_path": config_path,
        "present_before": present_before,
        "present_after": present_after,
        "attester_session_id": attester_session_id,
        "attester_witness": attester_witness,
    }
    if present_before:
        data["old_value"] = old_value
    if present_after:
        data["new_value"] = new_value
    if redacted:
        data["redacted"] = True
    return _build("config_write", "config", data)


def plan_stamped(
    *,
    plan_path: str,
    session_id: str,
    outcome: str,
    node_id: str | None = None,
    status_from: str | None = None,
    status_to: str | None = None,
    urls: list[str] | None = None,
    expected_url_count: int | None = None,
    reason: str | None = None,
    source: str = "target",
) -> dict[str, Any]:
    """Build the receipt emitted after a plan stamp is applied."""
    if outcome not in {"stamped", "idempotent_noop"}:
        raise ValidationError(f"unknown plan_stamped outcome: {outcome!r}")
    data: dict[str, Any] = {
        "plan_path": plan_path,
        "session_id": session_id,
        "outcome": outcome,
    }
    for key, value in (
        ("node_id", node_id),
        ("status_from", status_from),
        ("status_to", status_to),
        ("urls", urls),
        ("expected_url_count", expected_url_count),
        ("reason", reason),
    ):
        if value is not None:
            data[key] = value
    return _build("plan_stamped", source, data)


def plan_graduated(
    *,
    plan_path: str,
    outcome: str,
    session_id: str | None = None,
    node_id: str | None = None,
    status_from: str | None = None,
    status_to: str | None = None,
    urls: list[str] | None = None,
    expected_url_count: int | None = None,
    reason: str | None = None,
    source: str = "target",
) -> dict[str, Any]:
    """Build the receipt emitted after a plan graduate decision."""
    if outcome not in {"graduated", "idempotent_noop", "not_met"}:
        raise ValidationError(f"unknown plan_graduated outcome: {outcome!r}")
    data: dict[str, Any] = {"plan_path": plan_path, "outcome": outcome}
    for key, value in (
        ("session_id", session_id),
        ("node_id", node_id),
        ("status_from", status_from),
        ("status_to", status_to),
        ("urls", urls),
        ("expected_url_count", expected_url_count),
        ("reason", reason),
    ):
        if value is not None:
            data[key] = value
    return _build("plan_graduated", source, data)


class HermeticEscapeError(RuntimeError):
    """A test process tried to append to a journal outside its sandbox."""


def _hermetic_allowed_roots() -> list[Path]:
    """Roots a hermetic test process may legitimately write a journal into.

    ``fno.hermetic.neutralise`` pins ``FNO_EVENTS_PATH`` to a journal inside a
    throwaway sandbox, and every test that names its own file names one under
    ``tmp_path``. Both live under ``TMPDIR``. Both raw and resolved forms of
    each root are kept because macOS reports ``TMPDIR`` as ``/var/folders/...``
    while ``Path.resolve()`` yields ``/private/var/...``.

    ``HOME`` is deliberately NOT a root, even though neutralise points it at the
    sandbox. It is read at call time, so a test that restores the real ``HOME``
    (``test_provider_usage_live.py``, ``test_retask_live_smoke.py`` and
    ``test_roundtrip.py`` each do) would widen the allowed root to the whole
    home directory - which contains both the checkout and ``~/.fno``, the two
    files this guard exists to protect. Nothing is lost by dropping it: the
    sandbox HOME is created by ``tempfile.mkdtemp`` and so already sits under
    ``TMPDIR``.
    """
    roots: list[Path] = []
    candidates = [
        os.environ.get("TMPDIR") or "/tmp",
    ]
    pin = os.environ.get("FNO_EVENTS_PATH")
    # ABSOLUTE pins only. `Path("events.jsonl").parent` is `.`, and its realpath
    # is the process cwd - which under a bare `pytest` IS the checkout, so a
    # relative pin promoted the whole repository to an allowed root and turned
    # the guard off. `scripts/lib/events.sh` and `hooks/review-hold.sh` both
    # pass the pin through unvalidated, so this cannot assume a good value.
    # A relative pin is simply not a root; the write is judged on TMPDIR alone.
    if pin and Path(pin).is_absolute():
        candidates.append(str(Path(pin).parent))
    for raw in candidates:
        if not raw:
            continue
        base = Path(raw)
        for form in (base, Path(os.path.realpath(raw))):
            if form not in roots:
                roots.append(form)
    return roots


def _refuse_hermetic_escape(path: Path) -> None:
    """Refuse a journal write that would leave a test's sandbox.

    ``FNO_TEST_HERMETIC`` was a receipt nothing read. This is its first
    consumer, and it closes the class rather than one caller: a module that
    builds its own ``<root>/.fno/events.jsonl`` by hand consults neither
    ``FNO_EVENTS_PATH`` nor ``FNO_REPO_ROOT``, so no pin can reach it. Almost
    every event write in the tree funnels through :func:`append_event`, so the
    check belongs here - one guard, not one per path builder.

    ``spawn_think`` was the specimen: it wrote ``think_offered`` rows for the
    fixture node ``x-2222aaaa`` into the developer's live journal, and from
    there into the operator's needs panel. A worktree's ``.fno/events.jsonl``
    is a symlink to the canonical journal, so a worker's test run reached the
    operator's file from a directory nobody thought of as production.

    Scope, stated so the next reader does not overclaim it: this guards
    :func:`append_event` only. ``events/log.py`` and ``agents/events.py`` write
    journals through their own file handles and do not pass here. The rule
    itself lives in :func:`fno.hermetic.declared_root`, so this fence and the
    accessor fence in ``fno.paths`` cannot disagree.
    """
    from fno.hermetic import UndeclaredStateRootError, declared_root

    try:
        declared_root(path)
    except (HermeticEscapeError, UndeclaredStateRootError) as exc:
        # Both refusals get the journal's remedy: the undeclared one is a
        # SIBLING class, so catching only the escape sends the wrong advice.
        raise type(exc)(
            f"append_event refused a journal write with no declared root: "
            f"{path}. Pass an explicit events_path= under tmp_path, or resolve "
            "the journal with fno.paths.project_events_json() so "
            f"FNO_EVENTS_PATH applies. ({exc})"
        ) from exc


def append_event(
    event: dict[str, Any],
    events_path: Path | None = None,
    *,
    lock_timeout_seconds: float = 30,
) -> dict[str, Any] | None:
    """Commit an event to the authoritative store.

    Hands the exact envelope to the native binary's SQL transaction (WAL,
    FULL sync, positive readback); the store's judge decides the schema at
    commit, and a refused write raises :class:`ValidationError` carrying
    the one-line diagnostic. The retention class comes from the store, not
    a journal route, and there is no file fallback: a failed commit raises
    :class:`EventStoreUnavailable` instead of reporting a committed event.
    Returns the native receipt dict.

    ``events_path`` resolves the sibling store (``events.jsonl`` ->
    ``events.db`` beside it); it is a store locator, never an append target.
    The store judges the envelope at commit: a refused write raises
    :class:`ValidationError` carrying the one-line diagnostic.
    """
    if events_path is None:
        from fno.paths import project_events_json

        events_path = project_events_json()
    requested_path = Path(events_path)
    # Before the commit: a refused write must not leave a .fno/ behind either.
    _refuse_hermetic_escape(requested_path)
    from fno.events.store_client import EventStoreUnavailable, emit_envelope

    if requested_path.is_dir():
        # A directory at the journal path is a corrupt setup, and the store
        # would silently commit beside it (events.db strips the .jsonl stem),
        # reporting a mirrored receipt no reader can ever find there.
        raise EventStoreUnavailable(
            f"events path is a directory, not a journal: {requested_path}"
        )

    try:
        return emit_envelope(event, requested_path, timeout=lock_timeout_seconds)
    except EventStoreUnavailable:
        raise
    except ValidationError:
        # A judged refusal is the caller's named error class, never a store
        # failure: the broad catch below exists for everything else.
        raise
    except Exception as exc:  # noqa: BLE001 - one named failure class for callers
        raise EventStoreUnavailable(f"event store commit failed: {exc}") from exc


__all__ = [
    "ALLOWED_GATES",
    "ALLOWED_SOURCES",
    "ALLOWED_SOURCE_PATTERNS",
    "ENVELOPE_REQUIRED",
    "EVENT_TYPES",
    "MAX_DATA_BYTES",
    "RETENTION_DEFAULT",
    "RETENTION_MINIMUM_TTL_HOURS",
    "EPHEMERAL_SUFFIX",
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
    "context_snapshot",
    "done_race_collision",
    "integrity_warning",
    "MAIL_ESCALATION_REASONS",
    "mail_escalation",
    "mission_complete",
    "mission_started",
    "QUESTION_CAP",
    "operator_question",
    "operator_question_closed",
    "operator_decision",
    "decision_retracted",
    "phase_0_decision",
    "phase_transition",
    "session_satisfied",
    "retention_for",
    "EventStoreUnavailable",
    "validate",
    "validate_retention_schema",
    "FanInTally",
    "tally_fan_in",
    "verify_child_promise",
    "wave_advanced",
    "worktree_overlap_observed",
    "config_write",
    "plan_stamped",
    "plan_graduated",
    "agent_raw_inject",
]
