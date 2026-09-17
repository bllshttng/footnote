"""Tests for fno.agents.format — pure renderers.

Covers AC1-UI (--json shape), AC1-EDGE (empty), AC3-HP (cross-provider shape
stability), AC3-UI (jq round-trip parseable).
"""
from __future__ import annotations

import json
import pathlib

from fno.agents.format import (
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


def test_serialize_entry_reaches_a_real_liveness_origin() -> None:
    """The shared rule gates on `pid`, and this serializer must supply it.

    The shared corpus cannot catch this. Every case there hands `pid` in by
    hand, so it proves the RULE and says nothing about whether the caller
    feeds it. This row is the one `fno agents list` serves, and without the
    pid it read `null` / `pid-absent` forever while the corpus stayed green.
    """
    entry = _claude_entry(pid=4321, pid_start_time="2026-05-20T17:05:00Z")

    row = serialize_entry(entry, live_status="Working")

    assert row["liveness_origin"] == "survivor"
    assert row["liveness_origin_basis"] is None
    # pid is rust-only on the wire (schemas/agents-list-row.json), so the
    # internal input must not leak into the serialized row.
    assert "pid" not in row
    assert "pid_start_time" not in row


def test_serialize_entry_pidless_row_says_why_it_has_no_origin() -> None:
    entry = _claude_entry(pid=None, pid_start_time="2026-05-20T17:05:00Z")

    row = serialize_entry(entry, live_status="Working")

    assert row["liveness_origin"] is None
    assert row["liveness_origin_basis"] == "pid-absent"


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


def test_serialize_entry_emits_the_stored_effort_axis() -> None:
    """The list row exposes the selected effort, not a provider-derived guess."""
    row = serialize_entry(_codex_entry(effort="xhigh"), live_status=None)

    assert "effort" in row
    assert row["effort"] == "xhigh"


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
# Two serializers answer one question and they have drifted before, so both are
# pinned to schemas/agents-list-row.json: this file pins the Python side,
# crates/fno-agents/src/daemon.rs pins the Rust side, and a key added to one and
# not the other fails CI.
#
# The served `fno agents list` is the Rust projection; `serialize_entry` still
# feeds the field-coverage lint and must keep the shared key set.
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


def test_serialize_entry_emits_last_activity_basis() -> None:
    """The age's instrument rides the row on the Python lane too.

    The contract test above pins the KEY on both serializers; this pins the
    value lane: the resolver's instrument word when it answered, its reason
    word when it could not resolve the handle.
    """
    answered = serialize_entry(
        _claude_entry(),
        live_status=None,
        last_activity_age_s=30,
        last_activity_basis="last-entry",
    )
    assert answered["last_activity_basis"] == "last-entry"
    unresolvable = serialize_entry(
        _claude_entry(),
        live_status=None,
        last_activity_basis="resolver-error",
    )
    assert unresolvable["last_activity_basis"] == "resolver-error"


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


def test_serialize_entry_emits_thread_identity_without_a_mux_pane() -> None:
    entry = _codex_entry(fno_id="codex-thread", mux=None, substrate="thread")

    row = serialize_entry(entry, live_status=None)

    assert row["thread_id"] == "codex-thread"
    assert row["mux"] is None


def test_serialize_entry_emits_the_persisted_node() -> None:
    """A list row preserves the node already stored on the registry entry."""
    row = serialize_entry(_claude_entry(node="x-cafe"), live_status=None)

    assert row["node"] == "x-cafe"


def test_serialize_entry_keeps_unobserved_node_unknown() -> None:
    """A row with no measured provenance stays null; names are not a fallback."""
    row = serialize_entry(_claude_entry(node=None), live_status=None)

    assert row["node"] is None


def test_serialize_entry_carries_the_observed_model() -> None:
    """The derived reading reaches the row, and defaults to the same
    `no-transcript` the resolver reports when it finds no file -- never to a
    missing key, which an operator correctly reads as proving nothing."""
    observed = {"kind": "observed", "model": "glm-5.2", "samples": 300}

    row = serialize_entry(_claude_entry(), live_status=None, observed_model=observed)
    assert row["observed_model"] == observed

    default = serialize_entry(_claude_entry(), live_status=None)
    assert default["observed_model"] == {"kind": "no-transcript"}


def test_serialize_entry_provider_names_vendor_not_harness(_unused=None) -> None:
    """AC8, post x-f273: `provider` carries the stored vendor, never a harness.

    `provider: "claude"` on a zai-routed worker is what produced the wrong
    diagnosis this row shape was fixed for; the key was removed wholesale and
    the removal then hid the real v15+ vendor axis. The guard that survives is
    the one the original AC8 meant: the value under a vendor-named key must
    never be the harness.
    """
    row = serialize_entry(
        _claude_entry(provider="zai"), live_status=None
    )

    assert row["provider"] == "zai"
    assert row["provider"] != row["harness"]
    assert row["harness"] == "claude"
    assert "model" not in row
    assert "model_basis" not in row


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


# ---------------------------------------------------------------------------
# v23 (x-2019): the requested axis and the substitution marker on the row
# ---------------------------------------------------------------------------


def test_serialize_entry_carries_the_request_and_names_a_substitution() -> None:
    """A substituted row shows the verbatim request AND the marker naming both."""
    observed = {"kind": "observed", "model": "glm-5.3-flash", "samples": 31}

    row = serialize_entry(
        _claude_entry(requested_model="glm-5.3[1m]"),
        live_status=None,
        observed_model=observed,
    )
    assert row["requested_model"] == "glm-5.3[1m]"
    assert row["model_substituted"] == {
        "requested": "glm-5.3[1m]",
        "observed": "glm-5.3-flash",
    }


def test_serialize_entry_suffix_stripped_family_match_is_not_substituted() -> None:
    """glm-5.3[1m] requested vs glm-5.3 observed is a match: marker None."""
    observed = {"kind": "observed", "model": "glm-5.3", "samples": 8}

    row = serialize_entry(
        _claude_entry(requested_model="glm-5.3[1m]"),
        live_status=None,
        observed_model=observed,
    )
    assert row["requested_model"] == "glm-5.3[1m]"
    assert row["model_substituted"] is None


def test_serialize_entry_unknown_request_renders_null_not_clean() -> None:
    """No stored request: both keys ride the row, the marker reads null."""
    observed = {"kind": "observed", "model": "glm-5.3-flash", "samples": 8}

    row = serialize_entry(_claude_entry(), live_status=None, observed_model=observed)
    assert row["requested_model"] is None
    assert row["model_substituted"] is None

    no_transcript = serialize_entry(
        _claude_entry(requested_model="glm-5.3[1m]"), live_status=None
    )
    assert no_transcript["requested_model"] == "glm-5.3[1m]"
    assert no_transcript["model_substituted"] is None
