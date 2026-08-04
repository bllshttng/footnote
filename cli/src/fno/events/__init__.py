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
import math as _math
import re as _re
import sys as _sys
from pathlib import Path
from typing import Any, TypeGuard

import yaml as _yaml

from ..mutex import acquire_dir_mutex, release_dir_mutex
from .verify_child_promise import FanInTally, tally_fan_in, verify_child_promise


class ValidationError(Exception):
    """Raised when an event fails schema validation."""


class SchemaUnavailableError(Exception):
    """Raised when the schema manifest cannot be loaded at module import."""


def _is_nonnegative_integral_number(value: Any) -> TypeGuard[int | float]:
    if type(value) is int:
        return 0 <= value <= _sys.float_info.max
    if type(value) is float:
        return _math.isfinite(value) and value >= 0 and value.is_integer()
    return False


def _utc_timestamp(value: Any) -> _dt.datetime | None:
    if not isinstance(value, str) or not value:
        return None
    if _re.fullmatch(
        r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]{1,6})?(?:Z|\+00:00)",
        value,
    ) is None:
        return None
    try:
        parsed = _dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None or parsed.utcoffset() != _dt.timedelta(0):
        return None
    return parsed


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
DATA_SIZE_ENCODING: str
ALLOWED_SOURCES: set[str]
# x-2901: per-agent worker sources (worker:<id>, stream-worker:<id>) validate by
# regex, not enum membership. Compiled from envelope.properties.source.patterns.
ALLOWED_SOURCE_PATTERNS: list[Any]
ALLOWED_GATES: set[str]
_schema_load_error: SchemaUnavailableError | None = None

try:
    SCHEMA = _load_schema()
    EVENT_TYPES = {e["name"]: e for e in SCHEMA.get("event_types", [])}
    ENVELOPE_REQUIRED = SCHEMA["envelope"]["required"]
    MAX_DATA_BYTES = SCHEMA.get("limits", {}).get("max_data_bytes", 65536)
    DATA_SIZE_ENCODING = SCHEMA.get("limits", {}).get("data_size_encoding", "")
    if DATA_SIZE_ENCODING != "compact-json-ascii-v1":
        raise SchemaUnavailableError(
            "unsupported limits.data_size_encoding: "
            f"{DATA_SIZE_ENCODING!r}"
        )
    ALLOWED_SOURCES = set(SCHEMA["envelope"]["properties"]["source"]["enum"])
    ALLOWED_SOURCE_PATTERNS = [
        _re.compile(p)
        for p in SCHEMA["envelope"]["properties"]["source"].get("patterns", [])
    ]
    ALLOWED_GATES = set(SCHEMA.get("gates", []))
    # a2a status-breakpoint family (x-dbaf): types carrying the extended envelope.
    _family = SCHEMA.get("protocol_family", {})
    PROTOCOL_FAMILY_TYPES = set(_family.get("types", []))
    PROTOCOL_FAMILY_VERSION = _family.get("version", 1)
    PROTOCOL_ENVELOPE_ALLOWED = set(_family.get("envelope", {}).get("allowed", []))
    PROTOCOL_ENVELOPE_REQUIRED = list(_family.get("envelope", {}).get("required", []))
    PROTOCOL_OUTCOME_ENUM = set(_family.get("outcome", {}).get("enum", []))
    PROTOCOL_OUTCOME_ON = set(_family.get("outcome", {}).get("present_on", []))
except SchemaUnavailableError as _exc:
    SCHEMA = None
    EVENT_TYPES = None
    ENVELOPE_REQUIRED = []
    MAX_DATA_BYTES = 65536
    DATA_SIZE_ENCODING = ""
    ALLOWED_SOURCES = set()
    ALLOWED_SOURCE_PATTERNS = []
    ALLOWED_GATES = set()
    PROTOCOL_FAMILY_TYPES = set()
    PROTOCOL_FAMILY_VERSION = 1
    PROTOCOL_ENVELOPE_ALLOWED = set()
    PROTOCOL_ENVELOPE_REQUIRED = []
    PROTOCOL_OUTCOME_ENUM = set()
    PROTOCOL_OUTCOME_ON = set()
    _schema_load_error = _exc


MAX_SAFE_EVENT_INTEGER = 9_007_199_254_740_991


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
    source = event["source"]
    # isinstance guard first: a non-str source must reject cleanly, not crash
    # p.match() with a TypeError (the pre-pattern set-membership tolerated it).
    if not isinstance(source, str) or (
        source not in ALLOWED_SOURCES
        and not any(p.match(source) for p in ALLOWED_SOURCE_PATTERNS)
    ):
        raise ValidationError(
            f"unknown source: {source!r} "
            f"(allowed: {sorted(ALLOWED_SOURCES)} "
            f"or patterns {[p.pattern for p in ALLOWED_SOURCE_PATTERNS]})"
        )

    type_name = event["type"]
    assert EVENT_TYPES is not None  # _require_schema() above guarantees it is loaded
    if type_name not in EVENT_TYPES:
        raise ValidationError(f"unknown event type: {type_name}")

    type_spec = EVENT_TYPES[type_name]
    raw_data = event.get("data")
    if raw_data is not None and not isinstance(raw_data, dict):
        raise ValidationError("event data must be an object")
    data = raw_data or {}

    for field in type_spec.get("data", {}).get("required", []):
        if field == "gate" and not data.get("gate_bearing", False):
            continue
        if field not in data:
            raise ValidationError(
                f"event type {type_name} missing required data field: {field}"
            )

    # a2a status-breakpoint family (x-dbaf): the extended envelope. Routable
    # fields live at envelope level; additionalProperties:false for this family
    # ONLY (legacy types keep today's tolerance). Enforced pre-lock so a
    # malformed emit rejects before touching events.jsonl.
    if type_name in PROTOCOL_FAMILY_TYPES:
        for field in PROTOCOL_ENVELOPE_REQUIRED:
            if field not in event:
                raise ValidationError(
                    f"event type {type_name} missing required envelope field: {field}"
                )
        extra = set(event) - PROTOCOL_ENVELOPE_ALLOWED
        if extra:
            raise ValidationError(
                f"event type {type_name} has unknown envelope field(s): "
                f"{sorted(extra)} (allowed: {sorted(PROTOCOL_ENVELOPE_ALLOWED)})"
            )
        if event.get("v") != PROTOCOL_FAMILY_VERSION:
            raise ValidationError(
                f"event type {type_name} envelope v must be "
                f"{PROTOCOL_FAMILY_VERSION} (got {event.get('v')!r})"
            )
        has_outcome = "outcome" in event
        if type_name in PROTOCOL_OUTCOME_ON:
            if not has_outcome:
                raise ValidationError(
                    f"event type {type_name} requires envelope field: outcome"
                )
            if event["outcome"] not in PROTOCOL_OUTCOME_ENUM:
                raise ValidationError(
                    f"unknown {type_name} outcome: {event['outcome']!r} "
                    f"(allowed: {sorted(PROTOCOL_OUTCOME_ENUM)})"
                )
        elif has_outcome:
            raise ValidationError(
                f"event type {type_name} must not carry envelope field outcome "
                f"(allowed only on {sorted(PROTOCOL_OUTCOME_ON)})"
            )

    if type_name == "phase_transition" and data.get("gate_bearing") and not data.get("gate"):
        raise ValidationError(
            "phase_transition with gate_bearing=true must include data.gate"
        )

    if type_name == "context_snapshot":
        if source not in {"hook", "test"}:
            raise ValidationError(
                f"context_snapshot source must be hook or test (got {source!r})"
            )
        session_id = data.get("session_id")
        harness = data.get("harness")
        entry_state = data.get("entry_state")
        if not isinstance(session_id, str) or not session_id.strip():
            raise ValidationError("context_snapshot session_id cannot be empty")
        if not isinstance(harness, str) or harness not in {"claude", "codex", "gemini"}:
            raise ValidationError(f"unknown context_snapshot harness: {harness!r}")
        if not isinstance(entry_state, str) or entry_state not in {
            "startup",
            "resume",
            "clear",
            "post_compact",
        }:
            raise ValidationError(f"unknown context_snapshot entry_state: {entry_state!r}")
        manifest = data.get("source_manifest")
        errors = data.get("measurement_errors", [])
        if not isinstance(manifest, list) or not all(
            isinstance(item, dict) for item in manifest
        ):
            raise ValidationError("context_snapshot source_manifest must contain objects")
        if not isinstance(errors, list) or not all(isinstance(item, str) for item in errors):
            raise ValidationError("context_snapshot measurement_errors must contain strings")
        observed = [item for item in manifest if item.get("status") == "observed"]
        if any(
            not _is_nonnegative_integral_number(item.get("bytes"))
            or not isinstance(item.get("content_hash"), str)
            for item in observed
        ):
            raise ValidationError(
                "context_snapshot observed sources require nonnegative bytes and content_hash"
            )
        expected_bytes = sum(int(item["bytes"]) for item in observed)
        expected_hashes = [item["content_hash"] for item in observed]
        expected_context_hash = (
            _hashlib.sha256("\n".join(expected_hashes).encode()).hexdigest()
            if expected_hashes
            else None
        )
        context_bytes = data.get("context_bytes")
        estimated_tokens = data.get("estimated_tokens")
        if (
            not _is_nonnegative_integral_number(context_bytes)
            or int(context_bytes) != expected_bytes
        ):
            raise ValidationError("context_snapshot context_bytes disagrees with source_manifest")
        if (
            not _is_nonnegative_integral_number(estimated_tokens)
            or int(estimated_tokens) != (expected_bytes + 3) // 4
        ):
            raise ValidationError("context_snapshot estimated_tokens disagrees with context_bytes")
        if data.get("source_hashes") != expected_hashes:
            raise ValidationError("context_snapshot source_hashes disagree with source_manifest")
        if data.get("context_hash") != expected_context_hash:
            raise ValidationError("context_snapshot context_hash disagrees with source_hashes")
        if not isinstance(data.get("measurement_complete"), bool):
            raise ValidationError("context_snapshot measurement_complete must be boolean")
        complete = data.get("measurement_complete") is True
        all_observed = bool(manifest) and len(observed) == len(manifest)
        if complete != (all_observed and not errors):
            raise ValidationError(
                "context_snapshot completeness disagrees with manifest and errors"
            )

    if type_name == "verification_receipt":
        if _utc_timestamp(event["ts"]) is None:
            raise ValidationError(
                "verification_receipt envelope ts must be RFC3339 UTC"
            )
        type_sources = type_spec.get("sources", [])
        if source not in type_sources:
            raise ValidationError(
                f"verification_receipt does not allow source {source!r} "
                f"(allowed: {sorted(type_sources)})"
            )
        type_props = type_spec["data"]["properties"]
        mode = data.get("mode")
        result = data.get("result")
        if mode not in type_props["mode"]["enum"]:
            raise ValidationError(f"unknown verification_receipt data.mode: {mode!r}")
        if result not in type_props["result"]["enum"]:
            raise ValidationError(f"unknown verification_receipt data.result: {result!r}")
        candidate_sha = data.get("candidate_sha")
        if not isinstance(candidate_sha, str) or _re.fullmatch(
            r"[0-9a-f]{40}", candidate_sha, _re.IGNORECASE
        ) is None:
            raise ValidationError("verification_receipt candidate_sha must be full 40-hex")
        command = data.get("command")
        scope = data.get("scope")
        if not isinstance(command, list) or not command or len(command) > 4096 or not all(
            isinstance(item, str) and item and len(item.encode("utf-8")) <= 4096
            for item in command
        ):
            raise ValidationError("verification_receipt command must contain bounded argv strings")
        if not isinstance(scope, list) or not scope or len(scope) > 128 or not all(
            isinstance(item, str) and item and len(item.encode("utf-8")) <= 512
            for item in scope
        ):
            raise ValidationError("verification_receipt scope must contain bounded step names")
        environment = data.get("environment")
        if not isinstance(environment, dict) or not all(
            isinstance(environment.get(field), str) and environment[field].strip()
            for field in ("host", "platform", "runner")
        ):
            raise ValidationError(
                "verification_receipt environment requires host, platform, and runner"
            )
        producer = data.get("producer")
        if not isinstance(producer, dict) or not all(
            isinstance(producer.get(field), str) and producer[field].strip()
            for field in ("kind", "id")
        ):
            raise ValidationError("verification_receipt producer requires kind and id")
        started = _utc_timestamp(data.get("started_at"))
        finished = _utc_timestamp(data.get("finished_at"))
        if started is None or finished is None or finished < started:
            raise ValidationError(
                "verification_receipt timestamps must be ordered RFC3339 UTC"
            )
        expected = data.get("steps_expected")
        executed = data.get("steps_executed")
        generation = data.get("generation")
        if (
            not _is_nonnegative_integral_number(generation)
            or int(generation) < 1
            or int(generation) > MAX_SAFE_EVENT_INTEGER
            or not _is_nonnegative_integral_number(expected)
            or not _is_nonnegative_integral_number(executed)
            or int(executed) > int(expected)
            or int(expected) != len(scope)
        ):
            raise ValidationError("verification_receipt step counts are invalid")
        if mode == "full" and result == "passed" and (
            int(expected) == 0 or int(executed) != int(expected)
        ):
            raise ValidationError(
                "verification_receipt full pass requires every nonzero step"
            )
        if mode == "void" and result == "passed":
            raise ValidationError("verification_receipt void mode cannot pass")

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

    # Same chokepoint rationale as session_satisfied above: the generic emit
    # CLI is the only writer for two of the three human_touch emitters (the mux
    # shells out), so a typo'd source/resolution must fail here, not land.
    if type_name == "human_touch":
        type_props = type_spec["data"]["properties"]
        for field in ("source", "resolution"):
            allowed = type_props[field]["enum"]
            if data.get(field) not in allowed:
                raise ValidationError(
                    f"unknown human_touch data.{field}: {data.get(field)!r} "
                    f"(allowed: {allowed})"
                )

    # Same chokepoint rationale: skill_eval_finding's dimension/verdict drive
    # x-0ca7's downstream ranking logic, so a typo'd enum value must fail here
    # rather than silently landing as an unrecognized bucket.
    if type_name == "skill_eval_finding":
        type_props = type_spec["data"]["properties"]
        for field in ("dimension", "verdict"):
            allowed = type_props[field]["enum"]
            if data.get(field) not in allowed:
                raise ValidationError(
                    f"unknown skill_eval_finding data.{field}: {data.get(field)!r} "
                    f"(allowed: {allowed})"
                )

    # Same chokepoint rationale: review_attestation is a trust-core gate event
    # (x-e703). loop-check fail-closes on anything but an exact `pass`, but a
    # producer typo (`verdict: passs`) should fail LOUD at emit rather than land
    # a silently-never-satisfying record. The generic emit CLI is a writer, so
    # the enum must be enforced here, not only in the typed helper.
    if type_name == "review_attestation":
        allowed = type_spec["data"]["properties"]["verdict"]["enum"]
        if data.get("verdict") not in allowed:
            raise ValidationError(
                f"unknown review_attestation data.verdict: {data.get('verdict')!r} "
                f"(allowed: {allowed})"
            )

    # Same chokepoint rationale: mail_escalation's reason drives the overlay
    # evidence text and the question-vs-attended-miss split, so a typo'd reason
    # must fail loud here, not land as a silent bucket.
    if type_name == "mail_escalation":
        allowed = type_spec["data"]["properties"]["reason"]["enum"]
        if data.get("reason") not in allowed:
            raise ValidationError(
                f"unknown mail_escalation data.reason: {data.get('reason')!r} "
                f"(allowed: {allowed})"
            )

    # Same chokepoint rationale: gate_escape's reason drives the retro
    # autonomy-debt ranking (x-f894). A typo'd reason must fail CLOSED here so
    # it is loud, not a silent bucket - the design's #1 correctness invariant.
    if type_name == "gate_escape":
        allowed = type_spec["data"]["properties"]["reason"]["enum"]
        if data.get("reason") not in allowed:
            raise ValidationError(
                f"unknown gate_escape data.reason: {data.get('reason')!r} "
                f"(allowed: {allowed})"
            )

    # Same chokepoint rationale: post_merge_dispatch_receipt is the attribution
    # record for a merge hand-off, and its phase drives the reserved-before-
    # accepted lifecycle the seven-day observation relies on (x-a35a). The typed
    # emit_receipt helper constrains phase at call time, but the generic emit
    # path is also a writer, so a typo'd phase/route must fail loud here, not
    # land a silently-misattributed record.
    if type_name == "post_merge_dispatch_receipt":
        type_props = type_spec["data"]["properties"]
        for field in ("phase", "route"):
            allowed = type_props[field]["enum"]
            if data.get(field) not in allowed:
                raise ValidationError(
                    f"unknown post_merge_dispatch_receipt data.{field}: "
                    f"{data.get(field)!r} (allowed: {allowed})"
                )

    # worktree_overlap_observed is the recurrence key for `fno worktree
    # overlaps`: an empty or non-list peer_session_ids would record an overlap
    # with no peers, a self-contradiction that must fail loud at the chokepoint
    # (the generic emit CLI is a writer here) rather than land and inflate or
    # corrupt the fold. observation_id is a 64-hex sha256 digest; rejecting a
    # non-digest id here blocks a hand-crafted event from impersonating another
    # observation and skewing the recurrence fold.
    if type_name == "worktree_overlap_observed":
        peers = data.get("peer_session_ids")
        if not isinstance(peers, list) or not peers or not all(
            isinstance(p, str) and p for p in peers
        ):
            raise ValidationError(
                "worktree_overlap_observed peer_session_ids must be a non-empty "
                "list of non-empty strings"
            )
        oid = data.get("observation_id")
        if not isinstance(oid, str) or _re.fullmatch(r"[0-9a-f]{64}", oid) is None:
            raise ValidationError(
                "worktree_overlap_observed observation_id must be a 64-hex sha256 digest"
            )
        # Recompute the digest from the fields so a hand-crafted event cannot
        # carry another observation's id and skew the recurrence fold. The
        # digest helper is the single implementation; validate reuses it.
        repo = data.get("repository_key")
        wt = data.get("worktree_key")
        obs = data.get("observer_session_id")
        if not (
            isinstance(repo, str) and repo
            and isinstance(wt, str) and wt
            and isinstance(obs, str) and obs
        ):
            raise ValidationError(
                "worktree_overlap_observed requires non-empty repository_key, "
                "worktree_key, and observer_session_id strings"
            )
        if oid != _overlap_observation_id(repo, wt, obs, sorted(set(peers))):
            raise ValidationError(
                "worktree_overlap_observed observation_id does not match its fields"
            )

    try:
        _json.dumps(data, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        serialized = (
            _json.dumps(data, separators=(",", ":"), ensure_ascii=True)
            .replace("\x7f", "\\u007f")
            .encode("ascii")
        )
    except (TypeError, UnicodeError, ValueError) as exc:
        raise ValidationError(f"event data is not serializable: {exc}") from exc
    if len(serialized) > MAX_DATA_BYTES:
        raise ValidationError(
            f"event data exceeds max_data_bytes "
            f"(got {len(serialized)}, limit {MAX_DATA_BYTES})"
        )


def _ts_now() -> str:
    return (
        _dt.datetime.now(_dt.timezone.utc)
        .isoformat(timespec="microseconds")
        .replace("+00:00", "Z")
    )


def _build(
    type_name: str,
    source: str,
    data: dict[str, Any],
    envelope: dict[str, Any] | None = None,
) -> dict[str, Any]:
    event = {"ts": _ts_now(), "type": type_name, "source": source, "data": data}
    if envelope:
        # Extra top-level routable fields (x-dbaf protocol family). A None value
        # means "omit" (a non-session producer drops from/model entirely rather
        # than faking an empty string), so it is never written.
        for k, v in envelope.items():
            if v is not None:
                event[k] = v
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


MAIL_ESCALATION_REASONS = frozenset({"question", "attended-miss"})


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
    generic ``fno event emit`` CLI reaches ``validate()`` too, where the same
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


def done_race_collision(
    *,
    node_id: str,
    first_completed_at: str,
    second_attempt_at: str,
    source: str = "fno-loop",
) -> dict[str, Any]:
    """Build a ``done_race_collision`` event.

    Forensic notice that two ``fno done`` calls landed on the same node; the
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

    Unit-separator (\\x1f) delimiter so the join is unambiguous: two different
    tuples cannot collide by concatenating to the same bytes. The peer list is
    already sorted by the caller, so repeated deliveries of one observation
    yield one id."""
    blob = "\x1f".join(
        [repository_key, worktree_key, observer_session_id, *peer_session_ids]
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
        raise ValidationError(
            "worktree_overlap_observed observer_session_id cannot be empty"
        )
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
        raise ValidationError(
            "worktree_overlap_observed repository_key cannot be empty"
        )
    if not isinstance(worktree_key, str) or not worktree_key:
        raise ValidationError(
            "worktree_overlap_observed worktree_key cannot be empty"
        )
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


def append_event(
    event: dict[str, Any],
    events_path: Path | None = None,
    *,
    lock_timeout_seconds: float = 30,
) -> None:
    """Append a validated event to events.jsonl under a mkdir mutex.

    Validates the event before acquiring the lock so a malformed payload
    cannot block other writers. The mutex directory matches the convention
    used by ``scripts/migrate-events-shape.py`` and
    ``crates/fno-agents/src/claims.rs``, so cross-language callers serialize
    correctly on the same path.
    """
    validate(event)

    if events_path is None:
        events_path = Path(".fno/events.jsonl")
    events_path.parent.mkdir(parents=True, exist_ok=True)

    lock_dir = events_path.parent / (events_path.name + ".lock.d")
    token = acquire_dir_mutex(lock_dir, lock_timeout_seconds)
    if token is None:
        raise TimeoutError(f"events.jsonl lock timeout: {lock_dir}")

    try:
        with events_path.open("a", encoding="utf-8") as fh:
            fh.write(_json.dumps(event, separators=(",", ":")) + "\n")
    finally:
        release_dir_mutex(lock_dir, token)


__all__ = [
    "ALLOWED_GATES",
    "ALLOWED_SOURCES",
    "ALLOWED_SOURCE_PATTERNS",
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
    "context_snapshot",
    "done_race_collision",
    "integrity_warning",
    "MAIL_ESCALATION_REASONS",
    "mail_escalation",
    "mission_complete",
    "mission_started",
    "phase_0_decision",
    "phase_transition",
    "session_satisfied",
    "validate",
    "FanInTally",
    "tally_fan_in",
    "verify_child_promise",
    "wave_advanced",
    "worktree_overlap_observed",
]
