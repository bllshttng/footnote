"""CLI entry point for `python -m fno.events`.

Supports two flags:

--emit-schema
    Prints a JSON object describing the unified events.jsonl envelope
    schema and the list of known event type names, then exits 0.

--validate-event TYPE
    Reads one JSON event from stdin and validates it as TYPE. Shell
    contract (consumed by scripts/lib/events-validate.sh): rc 0 valid,
    rc 1 invalid record, rc 2 substrate failure (schema unavailable or
    stdin not one JSON object). Diagnostics go to stderr; nothing is
    written to stdout.

Both modes are read-only and side-effect-free: no files are written and
no global state is modified.
"""
from __future__ import annotations

import json
import sys


def _build_unified_envelope_schema() -> dict:
    """Build the unified events.jsonl envelope schema (x-2901).

    Describes {ts, type, source, data} with source as an anyOf of the enum
    and the worker patterns. Structurally equal to schemas/events-v3.json
    after doc-key stripping (the parity gate diffs them).
    """
    from fno.events import (  # noqa: PLC0415
        ALLOWED_SOURCE_PATTERNS,
        ALLOWED_SOURCES,
        _schema_load_error,
    )

    if _schema_load_error is not None:
        print(f"emit-schema: schema unavailable: {_schema_load_error}", file=sys.stderr)
        sys.exit(1)

    source_enum = sorted(ALLOWED_SOURCES) if ALLOWED_SOURCES else []
    source_anyof: list[dict] = [{"enum": source_enum}]
    source_anyof += [{"pattern": p.pattern} for p in ALLOWED_SOURCE_PATTERNS]

    return {
        "$comment": "Unified envelope (x-2901). Emitted by cli/src/fno/events/__init__.py.",
        "type": "object",
        "required": ["ts", "type", "source", "data"],
        "properties": {
            "ts": {
                "type": "string",
                "description": "UTC RFC3339 timestamp",
            },
            "type": {
                "type": "string",
                "description": "Event type name from events-schema.yaml event_types",
            },
            "source": {
                "type": "string",
                "anyOf": source_anyof,
                "description": "Producer identity: a fixed-string source or a per-agent worker",
            },
            "data": {
                "type": "object",
                "description": "Per-type payload object",
            },
        },
        "additionalProperties": True,
    }


def _collect_event_types() -> list[str]:
    """Return the sorted list of Python-emitted event type names.

    Excludes event types that are Rust-only (sources exclusively 'daemon',
    'subagent', or 'loop'). These are documented in events-schema.yaml for
    validator coverage but are not emitted by the Python side; including
    them would false-positive the parity check's collision detector.
    """
    from fno.events import SCHEMA  # noqa: PLC0415

    if not SCHEMA:
        return []

    # Rust-infrastructure sources: process identities used exclusively by the
    # Rust fno-agents supervisor. Event types whose ALL sources are within this
    # set were added to events-schema.yaml as documentation for Rust-emitted
    # events and are never emitted by the Python fno pipeline.
    rust_infra_sources = frozenset(["daemon", "subagent", "loop", "pr-heal"])

    result = []
    for entry in SCHEMA.get("event_types", []):
        sources = set(entry.get("sources", []))
        # Include only if at least one source is outside the Rust-infra set
        # (i.e., a Python pipeline emitter actually uses this event type).
        if sources - rust_infra_sources:
            result.append(entry["name"])
    return sorted(result)


def _validate_event_mode(type_hint: str) -> None:
    """Validate one JSON event read from stdin against the canonical schema.

    Exits 0 valid, 1 invalid record (missing/forbidden field named), 2
    substrate failure (schema unavailable, stdin not one JSON object).
    """
    from fno.events import (  # noqa: PLC0415
        SchemaUnavailableError,
        ValidationError,
        validate,
    )

    try:
        event = json.loads(sys.stdin.read())
    except json.JSONDecodeError as exc:
        print(f"validate-event: payload is not valid JSON: {exc}", file=sys.stderr)
        sys.exit(2)
    if not isinstance(event, dict):
        print("validate-event: payload must be a JSON object", file=sys.stderr)
        sys.exit(2)
    if event.get("type") != type_hint:
        print(
            f"validate-event: type hint {type_hint!r} does not match payload type "
            f"{event.get('type')!r}",
            file=sys.stderr,
        )
        sys.exit(1)
    try:
        validate(event)
    except SchemaUnavailableError as exc:
        print(f"validate-event: schema unavailable: {exc}", file=sys.stderr)
        sys.exit(2)
    except ValidationError as exc:
        print(f"validate-event: {exc}", file=sys.stderr)
        sys.exit(1)
    sys.exit(0)


def main() -> None:
    """Entry point for `python -m fno.events`."""
    args = sys.argv[1:]

    if "--validate-event" in args:
        idx = args.index("--validate-event")
        if len(args) <= idx + 1 or args[idx + 1].startswith("-"):
            print(
                "Usage: python -m fno.events --validate-event <type>",
                file=sys.stderr,
            )
            sys.exit(2)
        _validate_event_mode(args[idx + 1])

    if "--emit-schema" not in args:
        print(
            "Usage: python -m fno.events --emit-schema | --validate-event <type>",
            file=sys.stderr,
        )
        sys.exit(2)

    try:
        envelope_schema = _build_unified_envelope_schema()
        event_types = _collect_event_types()
    except Exception as exc:  # noqa: BLE001
        print(f"emit-schema error: {exc}", file=sys.stderr)
        sys.exit(1)

    output = {
        "envelope": envelope_schema,
        "event_types": event_types,
    }
    print(json.dumps(output, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
