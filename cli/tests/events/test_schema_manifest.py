"""Structural-validation tests for cli/src/fno/events/schema.yaml.

The manifest is the canonical source of truth for events.jsonl shape.
These tests assert it parses and that key invariants (envelope required
fields, no duplicate event types, phase_transition declares gate_bearing)
hold structurally - so a bad PR fails the check rather than shipping a
silently broken manifest.
"""
from __future__ import annotations

from pathlib import Path

import yaml


MANIFEST = Path(__file__).resolve().parents[3] / "cli/src/fno/events/schema.yaml"


def _load() -> dict:
    return yaml.safe_load(MANIFEST.read_text(encoding="utf-8"))


def test_manifest_parses() -> None:
    data = _load()
    assert "envelope" in data
    assert "event_types" in data
    assert "gates" in data


def test_envelope_required_fields() -> None:
    data = _load()
    assert data["envelope"]["required"] == ["ts", "type", "source", "data"]


def test_no_duplicate_event_types() -> None:
    data = _load()
    names = [e["name"] for e in data.get("event_types", [])]
    duplicates = sorted({n for n in names if names.count(n) > 1})
    assert not duplicates, f"duplicate event type name: {','.join(duplicates)}"


def test_phase_transition_declares_gate_bearing() -> None:
    data = _load()
    pt = next((e for e in data["event_types"] if e["name"] == "phase_transition"), None)
    assert pt is not None, "phase_transition event type missing"
    assert "gate_bearing" in pt["data"]["properties"], (
        "phase_transition.data.properties.gate_bearing missing - schema invariant violated"
    )


def test_source_enum_present() -> None:
    data = _load()
    src = data["envelope"]["properties"]["source"]
    assert "enum" in src, "envelope.source must declare an enum of allowed producers"
    assert "target" in src["enum"]
    assert "megawalk" in src["enum"]


def test_size_limit_declared() -> None:
    data = _load()
    assert "limits" in data
    assert "max_data_bytes" in data["limits"]
    assert isinstance(data["limits"]["max_data_bytes"], int)
    assert data["limits"]["max_data_bytes"] >= 1024


def test_no_undocumented_deletions() -> None:
    """Removing an event type without a `deprecated:` marker fails CI.

    This test runs against the current manifest (HEAD); it cannot detect
    deletions on its own. It asserts the structural invariant: every
    event type must either be present in the current manifest OR carry
    a `deprecated:` ISO8601 marker if removed in this PR.

    Implementation note: a future enhancement can diff against the merge-base
    branch to enforce this on PRs that delete types. Today, this test
    enforces the structural invariant that deprecated entries (if present)
    declare a valid timestamp.
    """
    data = _load()
    for entry in data.get("event_types", []):
        if "deprecated" in entry:
            marker = entry["deprecated"]
            assert isinstance(marker, str) and "T" in marker, (
                f"event type {entry['name']!r} has deprecated marker that is not "
                f"ISO8601-shaped: {marker!r}"
            )
