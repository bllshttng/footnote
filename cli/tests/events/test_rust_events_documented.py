"""Tests for Task 1.5: Rust supervisor events documented in events-schema.yaml.

Verifies that the Rust-emitted event kinds and the daemon/worker
sources are added additively to events-schema.yaml, and that the
existing entries are not changed.
"""
from __future__ import annotations

from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).resolve().parents[3]
SCHEMA_PATH = REPO_ROOT / "cli/src/fno/events/schema.yaml"

# Real Rust event kinds that must appear in events-schema.yaml after W7.
# The complete list is kept in sync with KNOWN_EVENT_KINDS in
# crates/fno-agents/src/lib.rs. To regenerate: grep .emit\( and .emit_fields\(
# across crates/fno-agents/src/*.rs and extract the first string argument.
RUST_EVENT_KINDS = [
    # Original 17 (W7 initial)
    "agent_spawned",
    "agent_stopped",
    "agent_exited",
    "agent_removed",
    "agent_inconsistent",
    "agent_ask_done",
    "channel_registered",
    "daemon_started",
    "daemon_exited",
    "daemon_idle_pending_exit",
    "daemon_shutting_down",
    "daemon_state",
    "drive_attached",
    "drive_detached",
    "drive_crashed",
    "reconcile_error",
    "event_payload_too_large",
    # 13 additional kinds found by sigma-review audit (W7 fix)
    "agent_create_no_session",
    "agent_orphan_reaped",
    "agent_orphan_state_archived",
    "agent_spawn_failed",
    "agent_stop_error",
    "agent_spawn_cwd_fallback",
    "daemon_recovery_error",
    "drive_force_close_timeout",
    "drive_keystroke_stepped",
    "drive_takeover_after_stale",
    "drive_watch_input_rejected",
    "reconcile_deferred",
    "reconcile_done",
    # Task 2.2 (US4): deliver RPC inject primitive
    "agent_deliver_injected",
    "agent_deliver_demoted",
    # ab-734fcd6c: claude stream-json adopt front door (fail-open claim note)
    "agent_stream_claim_unavailable",
]

# Source values that must be in the envelope.source enum
REQUIRED_SOURCES = [
    "target", "megawalk", "megatron", "fno-loop",
    "hook", "subagent", "migration", "test", "backlog",
    "daemon",  # added in W7
]

# Pre-existing Python event types that must still be present (additive-only check)
EXISTING_EVENT_TYPES = [
    "phase_transition",
    "child_promise",
    "mission_started",
    "wave_advanced",
    "mission_complete",
]


@pytest.fixture(scope="module")
def schema() -> dict:
    return yaml.safe_load(SCHEMA_PATH.read_text(encoding="utf-8"))


def test_schema_loads(schema: dict) -> None:
    """events-schema.yaml must parse as YAML."""
    assert isinstance(schema, dict)


def test_daemon_in_source_enum(schema: dict) -> None:
    """'daemon' must be in the envelope source enum (W7 additive)."""
    enum = schema["envelope"]["properties"]["source"]["enum"]
    assert "daemon" in enum, "daemon must be in source enum"


def test_existing_sources_preserved(schema: dict) -> None:
    """All pre-existing sources must still be in the enum (additive-only)."""
    enum = set(schema["envelope"]["properties"]["source"]["enum"])
    for src in REQUIRED_SOURCES:
        assert src in enum, f"source {src!r} removed from enum (additive-only rule)"


def test_rust_events_documented(schema: dict) -> None:
    """All Rust-emitted event kinds must appear as event_types entries."""
    documented = {e["name"] for e in schema.get("event_types", [])}
    for kind in RUST_EVENT_KINDS:
        assert kind in documented, f"Rust event kind {kind!r} not documented in events-schema.yaml"


def test_existing_event_types_preserved(schema: dict) -> None:
    """Pre-existing Python event types must still be present (additive-only)."""
    documented = {e["name"] for e in schema.get("event_types", [])}
    for name in EXISTING_EVENT_TYPES:
        assert name in documented, f"existing event type {name!r} was removed"


def test_rust_events_have_daemon_source(schema: dict) -> None:
    """Rust event entries must list 'daemon' (or 'subagent') as a source."""
    documented = {e["name"]: e for e in schema.get("event_types", [])}
    for kind in RUST_EVENT_KINDS:
        entry = documented.get(kind)
        if entry is None:
            continue  # caught by test_rust_events_documented
        sources = entry.get("sources", [])
        assert any(
            s in ("daemon", "subagent") for s in sources
        ), f"Rust event {kind!r} sources {sources!r} must include daemon or subagent"
