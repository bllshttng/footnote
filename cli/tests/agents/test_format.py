"""Tests for fno.agents.format — pure renderers.

Covers AC1-UI (--json shape), AC1-EDGE (empty), AC3-HP (cross-provider shape
stability), AC3-UI (jq round-trip parseable).
"""
from __future__ import annotations

import json
import pathlib
import re

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
        harness="claude",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/worker-frontend/output.jsonl",
        short_id="abc12345",
        created_at="2026-05-20T17:00:00Z",
        status="live",
        last_message_at="2026-05-20T17:30:12Z",
    )
    base.update(overrides)
    return AgentEntry(**base)


def _codex_entry(**overrides) -> AgentEntry:
    base = dict(
        name="worker-migration",
        harness="codex",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/worker-migration/output.jsonl",
        harness_session_id="codex-sess-xyz",
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
    assert row["harness"] == "claude"
    assert row["short_id"] == "abc12345"
    assert row["cwd"] == "/Users/foo/code/proj"
    assert row["status"] == "live"
    assert row["live_status"] == "Working"
    assert row["last_message_at"] == "2026-05-20T17:30:12Z"


def test_serialize_entry_codex_keeps_short_id_null_and_live_status_null() -> None:
    entry = _codex_entry()

    row = serialize_entry(entry, live_status=None)

    assert row["harness"] == "codex"
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
    row = serialize_entry(_codex_entry(harness_session_id=None), live_status=None)

    assert row["session_id"] is None


def test_serialize_entry_shape_is_stable_across_providers() -> None:
    """AC3-HP — JSON shape stable across providers (same key set)."""
    claude_row = serialize_entry(_claude_entry(), live_status="Working")
    codex_row = serialize_entry(_codex_entry(), live_status=None)

    assert set(claude_row.keys()) == set(codex_row.keys())
    assert {
        "name",
        "harness",
        "observed_model",
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
    assert "HARNESS" in out
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
    assert "HARNESS" in out
    # No data row implies no agent-name tokens; we don't have any to assert
    # absence of, so just confirm the call doesn't crash on empty input.


def test_render_table_does_not_crash_on_missing_last_message_at() -> None:
    """Domain pitfall — legacy v1 entries may have last_message_at=None."""
    entry = _claude_entry(last_message_at=None)
    rows = [serialize_entry(entry, live_status=None)]

    out = render_table(rows)

    # The renderer must handle None and emit a placeholder (e.g. '-').
    assert entry.name in out


def test_render_table_event_age_column_beside_status() -> None:
    """The transcript age renders as its own column next to STATUS, so
    a row whose store-read state says one thing and whose transcript says
    another shows both readings instead of resolving the conflict silently."""
    from datetime import datetime, timedelta, timezone

    fresh = (
        datetime.now(timezone.utc) - timedelta(seconds=5)
    ).strftime("%Y-%m-%dT%H:%M:%SZ")
    rows = [
        serialize_entry(_claude_entry(), live_status="Working", last_event_at=fresh)
    ]

    out = render_table(rows)

    assert "EVENT AGE" in out
    # A freshly-stamped event renders a wall-clock + relative token, not the
    # bare '-' an absent reading gets.
    assert fresh[11:19] in out
    match = re.search(r"\((\d+)s\)", out)
    assert match is not None
    assert 4 <= int(match.group(1)) <= 15


def test_render_table_event_age_absent_renders_dash_not_fresh() -> None:
    """The honest-limits rule from the node: a row with no transcript reading
    must render as unread ('-'), never as fresh - an absent reading is not
    evidence of health."""
    rows = [serialize_entry(_claude_entry(), live_status="Working")]

    out = render_table(rows)

    # Columnar slice, not a token split: "EVENT AGE" and "LAST MESSAGE" are
    # two-word headers, so the header line has more whitespace-separated
    # tokens than a data row has cells.
    header, data = out.splitlines()[0], out.splitlines()[1]
    idx = header.index("EVENT AGE")
    assert data[idx : idx + len("EVENT AGE")].strip() == "-"


def test_render_table_last_message_shows_transcript_text() -> None:
    """LAST MESSAGE carries the last transcript turn. The column spent
    its life wired to `last_message_at` - a timestamp, often null - so it never
    showed a message; a 429 or a wait line is the actual diagnostic."""
    rows = [
        serialize_entry(
            _claude_entry(name="short"),
            live_status=None,
            last_message="[tool_use: Bash] running the suite now",
        ),
        # Absent probe reading renders '-', and a long line is capped at 40
        # chars so one transcript line cannot own the table.
        serialize_entry(
            _claude_entry(name="long"),
            live_status=None,
            last_message="x" * 120,
        ),
    ]

    out = render_table(rows)

    assert "running the suite now" in out
    assert "…" in out
    long_cells = [ln for ln in out.splitlines() if "xxxx" in ln]
    assert len(long_cells[0].split()) > 1, "a capped message must not eat CWD"


def test_serialize_entry_carries_the_last_event_pair() -> None:
    """The stamp and the last-turn text reach the JSON row, and default to None
    (not to a fabricated fresh reading) when the probe never answered."""
    row = serialize_entry(
        _claude_entry(),
        live_status=None,
        last_event_at="2026-08-15T17:00:00+00:00",
        last_message="still on it",
    )
    assert row["last_event_at"] == "2026-08-15T17:00:00+00:00"
    assert row["last_message"] == "still on it"

    default = serialize_entry(_claude_entry(), live_status=None)
    assert default["last_event_at"] is None
    assert default["last_message"] is None


# ---------------------------------------------------------------------------
# Shared key-set contract
#
# `serialize_entry` is NOT what serves `fno agents list` — the Rust daemon's
# `agent.list` projection is — so the two serializers drifted and the stale one
# was the reachable one. These tests pin the Python side to
# schemas/agents-list-row.json; crates/fno-agents/src/daemon.rs pins the Rust
# side to the same file, so a key added to one and not the other fails CI.
# ---------------------------------------------------------------------------

_SCHEMA_PATH = (
    pathlib.Path(__file__).resolve().parents[3] / "schemas" / "agents-list-row.json"
)


def _contract() -> dict:
    return json.loads(_SCHEMA_PATH.read_text())


def test_serialize_entry_key_set_matches_shared_contract() -> None:
    contract = _contract()
    expected = set(contract["required"]) | set(contract["python_only"]["keys"])

    row = serialize_entry(_claude_entry(), live_status="Working")

    assert set(row) == expected


def test_serialize_entry_emits_identity_and_hosting_fields() -> None:
    """The three keys whose absence made a bound pane worker read as unhosted
    and unidentified. Presence in the contract is not enough — assert the
    values actually reach the row."""
    entry = _claude_entry(
        harness_session_id="e6f78b98-e594-47ed-ad81-84f8a78b8bb7",
        mux={"session": "main", "pane_id": 10},
        crown_level=1,
        crown_scope="epic-x",
        crown_grantor="king",
    )

    row = serialize_entry(entry, live_status=None)

    assert row["harness"] == "claude"
    assert row["harness_session_id"] == "e6f78b98-e594-47ed-ad81-84f8a78b8bb7"
    assert row["mux"] == {"session": "main", "pane_id": 10}
    assert row["crown"] == "L1 epic-x"


def test_serialize_entry_carries_the_observed_model() -> None:
    """The derived reading reaches the row, and defaults to the same
    `no-transcript` the resolver reports when it finds no file -- never to a
    missing key, which an operator correctly reads as proving nothing."""
    observed = {"kind": "observed", "model": "glm-5.2", "samples": 300}

    row = serialize_entry(_claude_entry(), live_status=None, observed_model=observed)
    assert row["observed_model"] == observed

    default = serialize_entry(_claude_entry(), live_status=None)
    assert default["observed_model"] == {"kind": "no-transcript"}


def test_serialize_entry_emits_no_key_that_names_a_vendor(_unused=None) -> None:
    """AC8: no key whose name says vendor and whose value is a harness.

    `provider: "claude"` on a zai-routed worker is what produced the wrong
    diagnosis this row shape was fixed for. Leaving it beside a correct
    `observed_model` would hand a reader two answers and no way to rank them.
    """
    row = serialize_entry(_claude_entry(), live_status=None)

    assert "provider" not in row
    assert row["harness"] == "claude"


# ---------------------------------------------------------------------------
# `address`: the form drain-self actually reads
#
# Before this key existed, every registry row on a live host carried
# `handle: null` and `short_id: null` and the table had no address column, so
# the only copyable identifier in a row was `name` -- and a name-lane durable
# write is the largest still-growing category of stranded mail on the bus.
# ---------------------------------------------------------------------------


def test_address_is_the_canonical_first_eight_of_the_session_id() -> None:
    """The address a row advertises must equal what `drain-self` computes for
    itself, which is ``canonical_handle(session_id)`` -- the first eight, NOT
    the retired ``<harness>-<short>`` form and NOT the friendly alias."""
    entry = _claude_entry(harness_session_id="E6F78B98-e594-47ed-ad81-84f8a78b8bb7")

    row = serialize_entry(entry, live_status=None)

    assert row["address"] == "e6f78b98"


def test_address_falls_back_to_the_transport_key_only_for_claude() -> None:
    """A claude row's ``short_id`` IS its first-eight, so it is an honest
    fallback when no full session id was recorded. A codex/opencode
    ``short_id`` is a daemon worker key, not an address, so guessing one there
    would advertise a key nothing drains."""
    claude = _claude_entry(harness_session_id=None, short_id="abc12345")
    assert serialize_entry(claude, live_status=None)["address"] == "abc12345"

    codex = _codex_entry(harness_session_id=None, short_id="abc12345")
    assert serialize_entry(codex, live_status=None)["address"] is None


def test_address_is_null_when_no_identity_was_recorded() -> None:
    """Absence is reported as absence. A row with nothing addressable must not
    invent a plausible-looking handle -- that is how a strand starts."""
    entry = _codex_entry(harness_session_id=None, short_id=None)

    assert serialize_entry(entry, live_status=None)["address"] is None


def test_table_shows_address_and_never_truncates_it() -> None:
    """A truncated address is a wrong address, so NAME and CWD absorb overflow
    and the address column does not."""
    row = serialize_entry(
        _claude_entry(
            name="a-very-long-worker-name-that-will-be-truncated-for-sure",
            cwd="/Users/foo/code/some/deeply/nested/project/path/that/is/long",
            harness_session_id="e6f78b98-e594-47ed-ad81-84f8a78b8bb7",
        ),
        live_status=None,
    )

    out = render_table([row], terminal_width=60)

    assert "ADDRESS" in out.splitlines()[0]
    assert "e6f78b98" in out


def test_discovered_lane_leads_with_the_address_not_the_label() -> None:
    """The alias is a display label. It led the discovered table, so it was the
    leftmost thing a reader copied, and it is not an address."""
    discovered = [
        {
            "handle": "etl-3ad1f42d",
            "session_id": "3ad1f42d-1111-2222-3333-444455556666",
            "short_id": "3ad1f42d",
            "status": "idle",
            "project": "etl",
            "cwd": "/Users/foo/code/etl",
        }
    ]

    out = render_table([], discovered=discovered)

    lines = out.splitlines()
    banner = next(i for i, ln in enumerate(lines) if "DISCOVERED LIVE" in ln)
    header = lines[banner + 1]
    assert header.index("ADDRESS") < header.index("LABEL")
    assert "3ad1f42d" in out
    assert "etl-3ad1f42d" in out
