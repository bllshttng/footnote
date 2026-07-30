"""Rust supervisor events are documented in events-schema.yaml.

Every kind in the Rust ``KNOWN_EVENT_KINDS`` const has a schema.yaml entry
listing a daemon-side source, and the pre-existing sources and Python event
types are still there (those two checks stay additive-only on purpose).

The kind list is parsed out of lib.rs, not restated here, so adding a kind on
the Rust side cannot leave this file quietly asserting nothing about it.
"""
from __future__ import annotations

import re
from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).resolve().parents[3]
SCHEMA_PATH = REPO_ROOT / "cli/src/fno/events/schema.yaml"
LIB_RS = REPO_ROOT / "crates/fno-agents/src/lib.rs"

# Floor for the parsed const size, guarding against a VACUOUS pass: a regex that
# matched the wrong block or nothing at all would otherwise leave every assertion
# below comparing against an empty list.
#
# Deliberately well below the real count (49 at the time of writing) rather than
# just under it. Tightening it to catch a parse that loses a handful of kinds also
# turns a legitimate removal of a handful into a false red, and the const-to-
# schema.yaml direction is independently checked by step 6 of
# scripts/check-event-schema-parity.sh, so this does not have to carry that too.
# Its one job is telling a broken parse from a working one.
MIN_EVENT_KINDS = 40

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


def parse_known_event_kinds(text: str) -> list[str]:
    """Pull the KNOWN_EVENT_KINDS entries out of lib.rs source text.

    A module-level function, not fixture-internal, so the floor below can be
    tested directly. An anti-vacuous guard that is only ever exercised on the
    happy path is the same unverified-gate shape this module exists to remove.
    """
    start = text.find("pub const KNOWN_EVENT_KINDS")
    assert start != -1, "KNOWN_EVENT_KINDS const not found"
    end = text.find("];", start)
    assert end != -1, "KNOWN_EVENT_KINDS block is unterminated"
    kinds = re.findall(r'"([A-Za-z0-9_]+)"', text[start:end])
    assert len(kinds) >= MIN_EVENT_KINDS, (
        f"parsed only {len(kinds)} kinds out of KNOWN_EVENT_KINDS, expected at "
        f"least {MIN_EVENT_KINDS}. The const was probably reformatted and this "
        f"parser needs updating. If instead kinds were legitimately removed and "
        f"the count is really this low, lower the floor deliberately - but check "
        f"the parse first, because that is the failure this floor is here for."
    )
    return kinds


@pytest.fixture(scope="module")
def rust_event_kinds() -> list[str]:
    """Rust event kinds, read out of KNOWN_EVENT_KINDS in lib.rs.

    Derived from source text rather than mirrored in a Python list. The mirror
    this replaced had drifted 16 entries behind the const while every test here
    kept passing: the checks are additive-only, so a kind missing from the
    mirror is a kind nothing asserts about.
    """
    # Assert, do not skip. The const lives in this repo, so absence means it
    # moved or the crate was renamed - and a skip is a green run, which would
    # quietly stop asserting the very thing this fixture exists to assert.
    assert LIB_RS.exists(), f"{LIB_RS} not found; did the crate move?"
    return parse_known_event_kinds(LIB_RS.read_text(encoding="utf-8"))


def test_floor_rejects_a_truncated_parse() -> None:
    """The anti-vacuous floor must actually trip, not just sit there.

    A reformat the regex cannot read yields a short list; without the floor that
    compares as equal-to-everything and every assertion below passes vacuously.
    """
    with pytest.raises(AssertionError, match="check the parse first"):
        parse_known_event_kinds(
            'pub const KNOWN_EVENT_KINDS: &[&str] = &[\n    "a", "b", "c",\n];\n'
        )


def test_unterminated_const_block_is_loud() -> None:
    """A const with no closing `];` must raise, never return a partial list."""
    with pytest.raises(AssertionError, match="unterminated"):
        parse_known_event_kinds('pub const KNOWN_EVENT_KINDS: &[&str] = &[\n    "a",\n')


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


def test_rust_events_documented(schema: dict, rust_event_kinds: list[str]) -> None:
    """All Rust-emitted event kinds must appear as event_types entries."""
    documented = {e["name"] for e in schema.get("event_types", [])}
    missing = [k for k in rust_event_kinds if k not in documented]
    assert not missing, (
        f"KNOWN_EVENT_KINDS entries not documented in {SCHEMA_PATH.name}: {missing}. "
        f"Add an event_types entry (name + sources) for each."
    )


def test_existing_event_types_preserved(schema: dict) -> None:
    """Pre-existing Python event types must still be present (additive-only)."""
    documented = {e["name"] for e in schema.get("event_types", [])}
    for name in EXISTING_EVENT_TYPES:
        assert name in documented, f"existing event type {name!r} was removed"


def test_rust_events_have_daemon_source(schema: dict, rust_event_kinds: list[str]) -> None:
    """Rust event entries must list 'daemon' (or 'subagent') as a source."""
    documented = {e["name"]: e for e in schema.get("event_types", [])}
    for kind in rust_event_kinds:
        entry = documented.get(kind)
        if entry is None:
            continue  # caught by test_rust_events_documented
        sources = entry.get("sources", [])
        assert any(
            s in ("daemon", "subagent") for s in sources
        ), f"Rust event {kind!r} sources {sources!r} must include daemon or subagent"
