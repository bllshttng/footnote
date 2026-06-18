"""Tests for fno.agents.format — pure renderers.

Covers AC1-UI (--json shape), AC1-EDGE (empty), AC3-HP (cross-provider shape
stability), AC3-UI (jq round-trip parseable).
"""
from __future__ import annotations

import json

import pytest

from fno.agents.format import (
    JSON_SCHEMA_VERSION,
    render_json,
    render_table,
    serialize_entry,
)
from fno.agents.registry import AgentEntry


def _claude_entry(**overrides) -> AgentEntry:
    base = dict(
        name="worker-frontend",
        provider="claude",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/worker-frontend/output.jsonl",
        claude_short_id="abc12345",
        created_at="2026-05-20T17:00:00Z",
        status="live",
        last_message_at="2026-05-20T17:30:12Z",
    )
    base.update(overrides)
    return AgentEntry(**base)


def _codex_entry(**overrides) -> AgentEntry:
    base = dict(
        name="worker-migration",
        provider="codex",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/worker-migration/output.jsonl",
        codex_session_id="codex-sess-xyz",
        created_at="2026-05-20T17:15:00Z",
        status="live",
        last_message_at="2026-05-20T17:15:43Z",
    )
    base.update(overrides)
    return AgentEntry(**base)


def test_serialize_entry_claude_includes_short_id_and_live_status() -> None:
    entry = _claude_entry()

    row = serialize_entry(entry, live_status="Working")

    assert row["name"] == "worker-frontend"
    assert row["provider"] == "claude"
    assert row["short_id"] == "abc12345"
    assert row["cwd"] == "/Users/foo/code/proj"
    assert row["status"] == "live"
    assert row["live_status"] == "Working"
    assert row["last_message_at"] == "2026-05-20T17:30:12Z"


def test_serialize_entry_codex_keeps_short_id_null_and_live_status_null() -> None:
    entry = _codex_entry()

    row = serialize_entry(entry, live_status=None)

    assert row["provider"] == "codex"
    assert row["short_id"] is None
    assert row["live_status"] is None


def test_serialize_entry_surfaces_codex_session_id_as_session_id() -> None:
    """Codex agents expose their resume target via the unified session_id key.

    Previously codex_session_id was stored but never surfaced in
    `fno agents list`, so the resume UUID (the argument `codex resume`
    needs) was invisible to JSON consumers. session_id resolves to the
    provider-specific id.
    """
    row = serialize_entry(_codex_entry(), live_status=None)

    assert row["session_id"] == "codex-sess-xyz"
    # short_id stays claude-only for back-compat.
    assert row["short_id"] is None


def test_serialize_entry_session_id_is_claude_short_id_for_claude() -> None:
    row = serialize_entry(_claude_entry(), live_status="Working")

    assert row["session_id"] == "abc12345"
    assert row["short_id"] == "abc12345"


def test_serialize_entry_session_id_none_when_uncaptured() -> None:
    """A codex entry whose session id was never captured reports None."""
    row = serialize_entry(_codex_entry(codex_session_id=None), live_status=None)

    assert row["session_id"] is None


def test_serialize_entry_shape_is_stable_across_providers() -> None:
    """AC3-HP — JSON shape stable across providers (same key set)."""
    claude_row = serialize_entry(_claude_entry(), live_status="Working")
    codex_row = serialize_entry(_codex_entry(), live_status=None)

    assert set(claude_row.keys()) == set(codex_row.keys())
    assert {
        "name",
        "provider",
        "short_id",
        "session_id",
        "cwd",
        "created_at",
        "last_message_at",
        "status",
        "live_status",
        "log_path",
    }.issubset(claude_row.keys())


def test_render_json_for_populated_registry() -> None:
    """AC1-UI — --json output contains the documented top-level keys."""
    rows = [
        serialize_entry(_claude_entry(), live_status="Working"),
        serialize_entry(_codex_entry(), live_status=None),
    ]
    filters = {"cwd": None, "provider": None, "status": None}

    out = render_json(rows, filters_applied=filters)
    parsed = json.loads(out)

    assert parsed["count"] == 2
    assert parsed["schema_version"] == JSON_SCHEMA_VERSION
    assert parsed["filters_applied"] == filters
    assert len(parsed["agents"]) == 2


def test_render_json_for_empty_registry() -> None:
    """AC1-EDGE — empty registry returns valid empty shape."""
    out = render_json([], filters_applied={"cwd": None, "provider": None, "status": None})
    parsed = json.loads(out)

    assert parsed["agents"] == []
    assert parsed["count"] == 0
    assert parsed["schema_version"] == JSON_SCHEMA_VERSION


def test_render_json_is_round_trip_parseable() -> None:
    """AC3-UI — output is valid JSON (jq round-trip)."""
    rows = [serialize_entry(_claude_entry(), live_status="Idle")]
    filters = {"cwd": "/Users/foo/code/proj", "provider": "claude", "status": "live"}

    out = render_json(rows, filters_applied=filters)
    # Round-trip: parse, dump, parse again — same shape.
    first = json.loads(out)
    second = json.loads(json.dumps(first))
    assert first == second


def test_render_json_filter_intersection_with_zero_matches() -> None:
    """AC3-EDGE — empty rows with non-null filters."""
    filters = {"cwd": "/nonexistent", "provider": "gemini", "status": None}

    out = render_json([], filters_applied=filters)
    parsed = json.loads(out)

    assert parsed["agents"] == []
    assert parsed["count"] == 0
    assert parsed["filters_applied"] == filters


def test_render_table_header_row_present() -> None:
    """AC1-HP — table has the documented header row."""
    rows = [
        serialize_entry(_claude_entry(), live_status="Working"),
    ]

    out = render_table(rows)

    # Header tokens — flexible to width-adjustment, strict on presence.
    assert "NAME" in out
    assert "PROVIDER" in out
    assert "STATUS" in out
    assert "LIVE" in out
    assert "LAST MESSAGE" in out
    assert "CWD" in out


def test_render_table_data_row_count_matches_entries() -> None:
    """AC1-HP — 3 registry entries → 3 data rows."""
    rows = [
        serialize_entry(_claude_entry(name="a"), live_status="Working"),
        serialize_entry(_codex_entry(name="b"), live_status=None),
        serialize_entry(
            _claude_entry(name="c", status="orphaned"), live_status=None
        ),
    ]

    out = render_table(rows)
    # Non-blank lines: 1 header + 3 data rows. There can be a separator row.
    body_lines = [ln for ln in out.splitlines() if ln.strip()]
    # At minimum: header + 3 data = 4 lines. Separator optional.
    assert len(body_lines) >= 4
    assert "a" in out
    assert "b" in out
    assert "c" in out


def test_render_table_shows_dash_for_null_live_status() -> None:
    """Codex / fallback entries get '-' in the LIVE column (AC1-HP)."""
    rows = [serialize_entry(_codex_entry(), live_status=None)]

    out = render_table(rows)

    # The codex entry's LIVE column should not contain 'Working' / 'Idle' /
    # 'Needs input'; the empty-marker is the literal '-'.
    assert "Working" not in out
    assert "Idle" not in out
    assert "Needs input" not in out
    assert " - " in out or out.rstrip().endswith("-")


def test_render_table_shows_orphan_status() -> None:
    """AC1-HP — orphaned entry's STATUS column shows 'orphan' (or 'orphaned')."""
    rows = [
        serialize_entry(
            _claude_entry(name="zombie", status="orphaned"), live_status=None
        ),
    ]

    out = render_table(rows)

    assert "orphan" in out.lower()


def test_render_table_for_empty_registry_emits_header_only() -> None:
    """Empty rows → header row + no data rows (still parseable shape)."""
    out = render_table([])

    assert "NAME" in out
    assert "PROVIDER" in out
    # No data row implies no agent-name tokens; we don't have any to assert
    # absence of, so just confirm the call doesn't crash on empty input.


def test_render_table_does_not_crash_on_missing_last_message_at() -> None:
    """Domain pitfall — legacy v1 entries may have last_message_at=None."""
    entry = _claude_entry(last_message_at=None)
    rows = [serialize_entry(entry, live_status=None)]

    out = render_table(rows)

    # The renderer must handle None and emit a placeholder (e.g. '-').
    assert entry.name in out
