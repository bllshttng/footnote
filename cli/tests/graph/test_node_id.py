"""Canonical node-ID module (ab-bbfccb8f, T1.2).

Covers the single source of truth in ``fno.graph._constants``: accessors,
``mint_node_id`` (configured scheme + collision retry + exhaustion), the
config-free ``is_wellformed_node_id`` matcher, ``extract_node_ids``, and
``node_id_suffix``.

Filter: `uv run pytest cli/tests -k node_id -q`
"""
from __future__ import annotations

import pytest

from fno.config import SettingsModel
from fno.graph import _constants as c


# --- is_wellformed_node_id (config-free, liberal) -------------------------


@pytest.mark.parametrize(
    "good",
    ["ab-55ba9adb", "xy-a3f9", "fno-abcd", "f-1234", "abcdefgh-12345678"],
)
def test_wellformed_accepts(good):
    assert c.is_wellformed_node_id(good)


@pytest.mark.parametrize(
    "bad",
    [
        "ab-123",          # 3 hex (< 4)
        "ab-123456789",    # 9 hex (> 8)
        "AB-12345678",     # uppercase prefix
        "ab-1234567g",     # non-hex char
        "a3f9c1d2",        # bare hex, no prefix-dash
        "1ab-1234",        # digit-led prefix
        "-12345678",       # empty prefix
        "",                # empty
    ],
)
def test_wellformed_rejects(bad):
    assert not c.is_wellformed_node_id(bad)


def test_wellformed_non_string_is_false():
    assert not c.is_wellformed_node_id(None)
    assert not c.is_wellformed_node_id(12345678)


def test_wellformed_is_liberal_about_sibling_families():
    # By design: the grammar matches sibling families too. Identity is a graph
    # lookup, not this predicate (AC4-ERR is enforced by graph-validation at the
    # call sites, not by narrowing the grammar).
    assert c.is_wellformed_node_id("cv-12345678")


# --- extract_node_ids ------------------------------------------------------


def test_extract_finds_configured_and_legacy():
    text = "promoted -> xy-a3f9 and the old -> ab-55ba9adb landed"
    assert c.extract_node_ids(text) == ["xy-a3f9", "ab-55ba9adb"]


def test_extract_skips_bare_hash():
    # A bare git short-hash has no prefix-dash -> not a candidate (AC4-ERR).
    assert c.extract_node_ids("commit a3f9c1d2 fixed it") == []


def test_extract_returns_sibling_candidates_for_caller_to_filter():
    # cv- is returned (liberal); the CALLER filters against graph keys.
    assert "cv-12345678" in c.extract_node_ids("see cv-12345678 carveout")


def test_extract_non_string_is_empty():
    assert c.extract_node_ids(None) == []


# --- node_id_suffix --------------------------------------------------------


def test_suffix_strips_prefix_at_dash_boundary():
    assert c.node_id_suffix("ab-a3f9c1d2") == "a3f9c1d2"
    assert c.node_id_suffix("xy-a3f9") == "a3f9"
    assert c.node_id_suffix("nodash") == "nodash"


# --- has_node_id_prefix (resolution-gate pre-check) -----------------------


def test_has_prefix_accepts_legacy_and_nonhex_legacy():
    assert c.has_node_id_prefix("ab-55ba9adb")
    assert c.has_node_id_prefix("ab-updatetest3")  # non-hex legacy/test id


def test_has_prefix_accepts_any_wellformed_past_prefix(monkeypatch):
    # codex P2: a node minted under a PAST configured prefix must still pass the
    # gate even when the CURRENT config uses a different prefix - the graph
    # lookup decides existence, not the gate.
    _patch_settings(monkeypatch, id_prefix="xy-", id_hex_width=4)
    assert c.has_node_id_prefix("fno-a3f9")   # different, but well-formed
    assert c.has_node_id_prefix("xy-a3f9")    # current configured
    assert c.has_node_id_prefix("ab-55ba9adb")  # legacy


def test_has_prefix_rejects_obvious_non_id(monkeypatch):
    _patch_settings(monkeypatch, id_prefix="xy-", id_hex_width=4)
    assert not c.has_node_id_prefix("just some words")
    assert not c.has_node_id_prefix("")
    assert not c.has_node_id_prefix(None)


# --- accessors: legacy fallback + configured ------------------------------


def _patch_settings(monkeypatch, **backlog):
    model = SettingsModel(config={"backlog": backlog})
    monkeypatch.setattr("fno.config.load_settings", lambda: model)


def test_prefix_legacy_fallback_when_unset(monkeypatch):
    _patch_settings(monkeypatch)  # no id_prefix -> None -> legacy
    assert c.node_id_prefix() == "ab-"


def test_hex_width_legacy_fallback_when_unset(monkeypatch):
    _patch_settings(monkeypatch)
    assert c.node_id_hex_width() == 8


def test_prefix_reads_configured(monkeypatch):
    _patch_settings(monkeypatch, id_prefix="xy-", id_hex_width=4)
    assert c.node_id_prefix() == "xy-"
    assert c.node_id_hex_width() == 4


def test_accessors_fall_back_when_load_raises(monkeypatch):
    # AC2-ERR: a malformed settings load must not crash the accessor.
    def boom():
        raise RuntimeError("malformed settings.yaml")

    monkeypatch.setattr("fno.config.load_settings", boom)
    assert c.node_id_prefix() == "ab-"
    assert c.node_id_hex_width() == 8


# --- mint_node_id ----------------------------------------------------------


def test_mint_uses_configured_scheme(monkeypatch):
    _patch_settings(monkeypatch, id_prefix="xy-", id_hex_width=4)
    mid = c.mint_node_id(set())
    assert c.is_wellformed_node_id(mid)
    assert mid.startswith("xy-")
    suffix = c.node_id_suffix(mid)
    assert len(suffix) == 4 and all(ch in "0123456789abcdef" for ch in suffix)


def test_mint_legacy_scheme_when_unconfigured(monkeypatch):
    _patch_settings(monkeypatch)
    mid = c.mint_node_id(set())
    assert mid.startswith("ab-")
    assert len(c.node_id_suffix(mid)) == 8


def test_mint_is_unique_against_existing(monkeypatch):
    _patch_settings(monkeypatch, id_prefix="xy-", id_hex_width=4)
    existing = {c.mint_node_id(set()) for _ in range(50)}
    fresh = c.mint_node_id(existing)
    assert fresh not in existing


def test_mint_raises_on_exhaustion(monkeypatch):
    # AC2-EDGE: every candidate "collides" -> bounded retries then a clear error.
    _patch_settings(monkeypatch, id_prefix="xy-", id_hex_width=4)
    monkeypatch.setattr(c, "_MINT_MAX_ATTEMPTS", 5)

    class AllContains:
        def __contains__(self, _x):
            return True

        def __len__(self):
            return 65536

    with pytest.raises(RuntimeError, match="exhaustion"):
        c.mint_node_id(AllContains())
