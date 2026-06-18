"""Tests for canonical JSON Schema files (Task 1.1).

Verifies that docs/architecture/schemas/events-v3.json and
status-v1.json parse as valid JSON Schema documents with the correct
structure.
"""
from __future__ import annotations

import json
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
SCHEMAS_DIR = REPO_ROOT / "docs/architecture/schemas"
EVENTS_V3 = SCHEMAS_DIR / "events-v3.json"
STATUS_V1 = SCHEMAS_DIR / "status-v1.json"

PYTHON_SOURCES = [
    "target", "megawalk", "megatron", "abi-loop",
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


def test_events_v3_has_oneof() -> None:
    """events-v3.json must have a top-level oneOf."""
    schema = json.loads(EVENTS_V3.read_text())
    assert "oneOf" in schema, "events-v3.json must have a top-level oneOf"
    assert len(schema["oneOf"]) == 2, "oneOf must have exactly 2 branches"


def test_events_v3_has_comment_about_bridge() -> None:
    """events-v3.json must have a $comment explaining the oneOf bridge."""
    schema = json.loads(EVENTS_V3.read_text())
    assert "$comment" in schema, "events-v3.json must have a $comment"


def test_events_v3_branch_a_python() -> None:
    """Branch A must require [ts, type, source, data] and exclude kind."""
    schema = json.loads(EVENTS_V3.read_text())
    branches = schema["oneOf"]
    branch_a = next(
        (b for b in branches if "type" in b.get("required", [])),
        None,
    )
    assert branch_a is not None, "no branch with required=[type] found"
    required = set(branch_a["required"])
    assert {"ts", "type", "source", "data"} == required
    # must have a not: {required: [kind]} discriminator
    assert "not" in branch_a, "branch A must have a 'not' discriminator"


def test_events_v3_branch_a_source_enum_contains_python_sources() -> None:
    """Branch A source must enumerate the Python sources."""
    schema = json.loads(EVENTS_V3.read_text())
    branches = schema["oneOf"]
    branch_a = next(b for b in branches if "type" in b.get("required", []))
    source_prop = branch_a["properties"]["source"]
    enum_vals = source_prop.get("enum", [])
    for src in PYTHON_SOURCES:
        assert src in enum_vals, f"source enum missing {src!r}"


def test_events_v3_branch_b_rust() -> None:
    """Branch B must require [ts, kind, source] and exclude type."""
    schema = json.loads(EVENTS_V3.read_text())
    branches = schema["oneOf"]
    branch_b = next(
        (b for b in branches if "kind" in b.get("required", [])),
        None,
    )
    assert branch_b is not None, "no branch with required=[kind] found"
    required = set(branch_b["required"])
    assert {"ts", "kind", "source"} == required
    # must have a not: {required: [type]} discriminator
    assert "not" in branch_b, "branch B must have a 'not' discriminator"


def test_events_v3_branch_b_source_pattern() -> None:
    """Branch B source must have a pattern matching daemon|worker:<id>."""
    schema = json.loads(EVENTS_V3.read_text())
    branches = schema["oneOf"]
    branch_b = next(b for b in branches if "kind" in b.get("required", []))
    source_prop = branch_b["properties"]["source"]
    pattern = source_prop.get("pattern", "")
    assert "daemon" in pattern, "source pattern must include 'daemon'"
    assert "worker" in pattern, "source pattern must include 'worker'"


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
