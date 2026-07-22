"""Tests for canonical JSON Schema files (Task 1.1).

Verifies that schemas/events-v3.json and status-v1.json parse as valid
JSON Schema documents with the correct structure.
"""
from __future__ import annotations

import json
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
SCHEMAS_DIR = REPO_ROOT / "schemas"
EVENTS_V3 = SCHEMAS_DIR / "events-v3.json"
STATUS_V1 = SCHEMAS_DIR / "status-v1.json"

PYTHON_SOURCES = [
    "target", "megawalk", "megatron", "fno-loop",
    "hook", "subagent", "migration", "test", "backlog",
]

AGENT_STATUS_VALUES = [
    "spawning", "ready", "idle", "busy", "live",
    "restarting", "orphaned", "failed", "exited", "permanent_dead",
]

AGENT_STATE_REQUIRED = ["schema_version", "short_id", "status"]


# ---------------------------------------------------------------------------
# AC1-HP: Canonical schemas exist and parse
# ---------------------------------------------------------------------------

def test_events_v3_exists() -> None:
    """events-v3.json must exist in the schemas directory."""
    assert EVENTS_V3.is_file(), f"missing: {EVENTS_V3}"


def test_status_v1_exists() -> None:
    """status-v1.json must exist in the schemas directory."""
    assert STATUS_V1.is_file(), f"missing: {STATUS_V1}"


def test_events_v3_parses_as_json() -> None:
    schema = json.loads(EVENTS_V3.read_text())
    assert isinstance(schema, dict)


def test_status_v1_parses_as_json() -> None:
    schema = json.loads(STATUS_V1.read_text())
    assert isinstance(schema, dict)


def test_events_v3_is_single_envelope() -> None:
    """The W7 oneOf split-brain is retired: one unified envelope, no oneOf."""
    schema = json.loads(EVENTS_V3.read_text())
    assert "oneOf" not in schema, "events-v3.json must be a single envelope"
    required = set(schema.get("required", []))
    assert {"ts", "type", "source", "data"} == required


def test_events_v3_has_comment_about_retirement() -> None:
    """events-v3.json must have a $comment explaining the single envelope."""
    schema = json.loads(EVENTS_V3.read_text())
    assert "$comment" in schema, "events-v3.json must have a $comment"


def test_events_v3_source_enum_contains_sources() -> None:
    """The unified source anyOf must enumerate the fixed-string sources."""
    schema = json.loads(EVENTS_V3.read_text())
    branches = schema["properties"]["source"]["anyOf"]
    enum_vals = next((b["enum"] for b in branches if "enum" in b), [])
    for src in [*PYTHON_SOURCES, "daemon", "active-backlog"]:
        assert src in enum_vals, f"source enum missing {src!r}"


def test_events_v3_source_pattern_matches_workers() -> None:
    """The unified source anyOf must carry the worker pattern branch."""
    schema = json.loads(EVENTS_V3.read_text())
    branches = schema["properties"]["source"]["anyOf"]
    pattern = next((b["pattern"] for b in branches if "pattern" in b), "")
    assert "worker" in pattern, "source pattern must match worker:<id>"
    assert "stream-worker" in pattern, "source pattern must match stream-worker:<id>"


def test_status_v1_required_fields() -> None:
    """status-v1.json must require schema_version, short_id, status."""
    schema = json.loads(STATUS_V1.read_text())
    required = set(schema.get("required", []))
    for field in AGENT_STATE_REQUIRED:
        assert field in required, f"status-v1.json must require {field!r}"


def test_status_v1_status_enum() -> None:
    """status-v1.json status property must enumerate all AgentStatus values."""
    schema = json.loads(STATUS_V1.read_text())
    status_enum = schema["properties"]["status"]["enum"]
    for val in AGENT_STATUS_VALUES:
        assert val in status_enum, f"status enum missing {val!r}"


def test_status_v1_pty_is_nullable() -> None:
    """status-v1.json pty property must allow null (no PTY) or object."""
    schema = json.loads(STATUS_V1.read_text())
    pty_prop = schema["properties"].get("pty", {})
    # Acceptable shapes: {"type": ["object", "null"]} OR {"oneOf": [...]}
    pty_types = pty_prop.get("type", [])
    pty_oneof = pty_prop.get("oneOf", [])
    has_null = "null" in pty_types or any(
        b.get("type") == "null" for b in pty_oneof
    )
    assert has_null, "pty property must allow null"


def test_status_v1_optional_fields_in_properties() -> None:
    """Optional AgentState fields must appear in properties."""
    schema = json.loads(STATUS_V1.read_text())
    props = set(schema.get("properties", {}).keys())
    optional = [
        "ready", "last_message_at", "last_reply",
        "restart_count", "last_restart_at", "pty",
    ]
    for field in optional:
        assert field in props, f"status-v1.json missing property {field!r}"
