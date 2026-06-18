"""CLI entry point for `python -m fno.events`.

Supports a single flag: --emit-schema

Prints a JSON object describing the Branch A (Python/fno) envelope schema
and the list of known event type names, then exits 0. Diagnostics go to
stderr; only the JSON schema goes to stdout so callers can pipe safely.

This module is read-only and side-effect-free: it does not write any files
or modify any global state.
"""
from __future__ import annotations

import json
import sys


def _build_branch_a_schema() -> dict:
    """Build the Branch A envelope schema for Python/fno events.

    This describes the Python emitter's envelope shape: {ts, type, source,
    data}. It mirrors docs/architecture/schemas/events-v3.json Branch A.
    """
    from fno.events import ALLOWED_SOURCES, _schema_load_error  # noqa: PLC0415

    if _schema_load_error is not None:
        print(f"W7 emit-schema: schema unavailable: {_schema_load_error}", file=sys.stderr)
        sys.exit(1)

    source_enum = sorted(ALLOWED_SOURCES) if ALLOWED_SOURCES else []

    return {
        "$comment": "Branch A: Python/fno envelope. Emitted by cli/src/fno/events/__init__.py.",
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
                "enum": source_enum,
                "description": "Producer identity for the Python/fno emitter",
            },
            "data": {
                "type": "object",
                "description": "Per-type payload object",
            },
        },
        "not": {"required": ["kind"]},
        "additionalProperties": True,
    }


def _collect_event_types() -> list[str]:
    """Return the sorted list of Python-emitted event type names.

    Excludes event types that are Rust-only (sources exclusively 'daemon'
    or 'subagent'). These are documented in events-schema.yaml for
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
    rust_infra_sources = frozenset(["daemon", "subagent"])

    result = []
    for entry in SCHEMA.get("event_types", []):
        sources = set(entry.get("sources", []))
        # Include only if at least one source is outside the Rust-infra set
        # (i.e., a Python pipeline emitter actually uses this event type).
        if sources - rust_infra_sources:
            result.append(entry["name"])
    return sorted(result)


def main() -> None:
    """Entry point for `python -m fno.events`."""
    args = sys.argv[1:]

    if "--emit-schema" not in args:
        print(
            "Usage: python -m fno.events --emit-schema",
            file=sys.stderr,
        )
        sys.exit(2)

    try:
        envelope_schema = _build_branch_a_schema()
        event_types = _collect_event_types()
    except Exception as exc:  # noqa: BLE001
        print(f"W7 emit-schema error: {exc}", file=sys.stderr)
        sys.exit(1)

    output = {
        "envelope": envelope_schema,
        "event_types": event_types,
    }
    print(json.dumps(output, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
