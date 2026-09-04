"""The features dimension beside the keystrokes (schema, both legs).

The features sub-table is what a harness can DO - a review command, an
RPC surface, a plugin system - held beside the pane mechanics, never
instead of them. These tests pin the closed vocabularies, the
probe-coupling refusals, and the unmeasured-by-absence reading. The Rust
leg's own refusals live inline in
crates/fno-agents/src/harness_capabilities.rs; the packaged-table
mutations here hit the same file that leg embeds, so a mutation that the
Python validator passes and the Rust one refuses would surface as a CI
failure on the Rust side, and the parity tests at the bottom pin the
shared answers directly.
"""
from __future__ import annotations

from importlib.resources import files

import pytest

from fno.agents.harness_map import (
    DispatchResolveError,
    MAP_VERSION,
    feature_claim,
    parse_capability_contract,
)

FEATURE_KEYS = {
    "review", "spawn", "attach", "rpc", "server", "plugins", "hooks",
    "skills_dir", "subagent_dispatch", "mcp", "acp",
}
FEATURE_STATES = {"native", "capable", "absent", "unmeasured"}


def _packaged_text() -> str:
    return files("fno.agents").joinpath("harness_capabilities.toml").read_text(encoding="utf-8")


# A used feature key the packaged table carries no stanza for, so appending
# a claim at EOF cannot collide with an existing table definition (TOML
# refuses a duplicate table header long before our validators run).
_UNUSED_KEY = "mcp"


def _with_decl_removed(text: str, key: str) -> str:
    """Drop ``key``'s probe declaration by renaming it out of the features
    namespace. A declaration is matched by header, never by body, so the
    test survives an edit to the declaration's marker or reason."""
    return text.replace(f'[probe."features.{key}"]', f'[probe."{key}-decl-removed"]', 1)


def test_feature_state_vocabulary_is_closed_naming_harness_key_value():
    bad = _packaged_text() + (
        f"\n[harness.claude.features.{_UNUSED_KEY}]\nstate = \"wired\"\n"
    )
    with pytest.raises(DispatchResolveError) as excinfo:
        parse_capability_contract(bad)
    message = str(excinfo.value)
    assert "claude" in message
    assert _UNUSED_KEY in message
    assert "wired" in message


def test_feature_key_set_is_closed_naming_harness_and_key():
    bad = _packaged_text() + "\n[harness.claude.features.spwan]\nstate = \"native\"\n"
    with pytest.raises(DispatchResolveError) as excinfo:
        parse_capability_contract(bad)
    message = str(excinfo.value)
    assert "claude" in message
    assert "spwan" in message


def test_a_feature_claim_carries_only_state():
    bad = _packaged_text() + (
        f"\n[harness.claude.features.{_UNUSED_KEY}]\n"
        "state = \"unmeasured\"\nevidence = \" vibes\"\n"
    )
    with pytest.raises(DispatchResolveError) as excinfo:
        parse_capability_contract(bad)
    assert "only state" in str(excinfo.value)


def test_a_used_feature_needs_its_probe_declaration_naming_the_key():
    bad = _with_decl_removed(_packaged_text(), _UNUSED_KEY) + (
        f"\n[harness.claude.features.{_UNUSED_KEY}]\nstate = \"unmeasured\"\n"
    )
    with pytest.raises(DispatchResolveError) as excinfo:
        parse_capability_contract(bad)
    message = str(excinfo.value)
    assert "claude" in message
    assert f"probe.features.{_UNUSED_KEY}" in message


def test_a_probe_declaration_may_only_name_a_feature_key():
    bad = _packaged_text() + (
        "\n[probe.\"features.spwan\"]\nkind = \"declared\"\n"
        "authority = \"{bin} --help\"\npattern = \"spwan\"\n"
    )
    with pytest.raises(DispatchResolveError) as excinfo:
        parse_capability_contract(bad)
    message = str(excinfo.value)
    assert "spwan" in message
    assert "feature keys" in message


def test_an_unprobeable_feature_declaration_needs_its_reason():
    # All ten feature keys already carry declarations, so the injected one
    # uses an out-of-set name: the instrument-shape check is namespace-
    # agnostic and fires before the closed-key check either leg runs.
    bad = _packaged_text() + "\n[probe.\"features.spwan\"]\nkind = \"unprobeable\"\n"
    with pytest.raises(DispatchResolveError) as excinfo:
        parse_capability_contract(bad)
    assert "reason" in str(excinfo.value)


def test_a_row_without_a_features_table_loads_with_no_refusal():
    # agy carries the smallest stanza in the table; strip it and the row
    # is back to the pre-features shape, which every reader must accept.
    stanza = "[harness.agy.features.spawn]\nstate = \"absent\""
    text = _packaged_text()
    assert stanza in text, "agy's spawn stanza moved; retarget this test"
    version, harnesses = parse_capability_contract(text.replace(stanza + "\n", "", 1))
    assert version >= 1
    assert "features" not in harnesses["agy"]


def test_every_feature_key_was_declared_probeable_or_excused():
    # The declaration set is written once and applied to every harness:
    # all ten keys carry an instrument. A missing one is a gap the probe
    # cannot even report as UNDECLARED, because the key would not exist.
    # This test reads the shipped table, not a mutation, so the packaged
    # file itself is what stays complete.
    text = _packaged_text()
    _, harnesses = parse_capability_contract(text)
    import tomllib

    probe = tomllib.loads(text).get("probe") or {}
    declared = {field[len("features."):] for field in probe if field.startswith("features.")}
    assert declared == FEATURE_KEYS


def test_map_version_floor_carries_the_features_shape():
    # A reader on an older packaged copy must not silently read a row
    # shape it does not know; the features sub-table arrived at this
    # version and every later schema change bumps again.
    assert MAP_VERSION >= 16


def test_feature_claim_reads_unmeasured_for_a_key_absent_from_the_row():
    # The honest default is loud: a feature nobody measured reads
    # unmeasured, it does not inherit a neighbour's answer. Every row
    # carries review and spawn measured; the eight catalog keys are
    # unmeasured until somebody runs the probe.
    for harness in ("claude", "agy"):
        assert feature_claim(harness, _UNUSED_KEY) == "unmeasured"


def test_feature_claim_refuses_an_unknown_key():
    with pytest.raises(DispatchResolveError) as excinfo:
        feature_claim("claude", "spwan")
    assert "spwan" in str(excinfo.value)
    assert "review" in str(excinfo.value)


def test_feature_claim_refuses_an_unknown_harness():
    with pytest.raises(DispatchResolveError):
        feature_claim("no-such-harness", "spawn")
