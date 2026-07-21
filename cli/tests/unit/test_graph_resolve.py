"""Unit tests for the slug/bare-hex resolver + high-recall search (ab-f82e8083).

resolve_node implements the deterministic resolution tiers 1-3 (exact ab-id,
exact slug, bare-8-hex re-prefix); search_entries is the high-recall describe-it
candidate generator over title+slug+details. Pure functions; no I/O.
"""
from __future__ import annotations

from fno.graph.fuzzy import resolve_node, search_entries


def _node(id, title, *, slug=None, details=None, status="ready"):
    n = {"id": id, "title": title, "status": status}
    if slug is not None:
        n["slug"] = slug
    if details is not None:
        n["details"] = details
    return n


ENTRIES = [
    _node("ab-994222ee", "dashless mobile grammar", slug="dashless-spawn",
          details="iOS autocorrect mangles ab- prefixes on a phone"),
    _node("ab-1234abcd", "Billing rebuild", slug="billing-rebuild"),
    _node("ab-deadbeef", "Docs refactor", slug="docs-refactor", status="done"),
]


# -- resolve_node: tier 1 exact ab-id ----------------------------------------


def test_tier1_exact_ab_id():
    m = resolve_node("ab-994222ee", ENTRIES)
    assert m.kind == "exact"
    assert m.id == "ab-994222ee"


# -- resolve_node: tier 2 exact slug -----------------------------------------


def test_tier2_exact_slug_resolves_to_id():
    # AC1-HP: an exact slug resolves to its ab-id.
    m = resolve_node("dashless-spawn", ENTRIES)
    assert m.kind == "exact"
    assert m.id == "ab-994222ee"


def test_tier2_slug_resolves_even_for_done_node():
    m = resolve_node("docs-refactor", ENTRIES)
    assert m.kind == "exact"
    assert m.id == "ab-deadbeef"


def test_tier2_slug_is_case_insensitive():
    # Mobile auto-capitalizes the first letter; a typed `Dashless-spawn` must
    # still resolve to the lowercase-stored slug (gemini review).
    for q in ("Dashless-spawn", "DASHLESS-SPAWN", "Dashless-Spawn"):
        m = resolve_node(q, ENTRIES)
        assert m.kind == "exact", q
        assert m.id == "ab-994222ee", q


def test_tier2_slug_starting_with_ab_prefix_resolves():
    # A title like "AB test cleanup" slugifies to `ab-test-cleanup`; the exact
    # slug tier must catch it (it is NOT a malformed ab-id) (codex P2).
    entries = [_node("ab-77777777", "AB test cleanup", slug="ab-test-cleanup")]
    m = resolve_node("ab-test-cleanup", entries)
    assert m.kind == "exact"
    assert m.id == "ab-77777777"


def test_tier2_unknown_slug_is_none():
    # AC1-ERR: a slug nobody has misses exact resolution (caller escalates to
    # describe-it), it does NOT silently fuzzy-match.
    m = resolve_node("nonsense-slug", ENTRIES)
    assert m.kind == "none"


# -- resolve_node: tier 3 bare-8-hex re-prefix -------------------------------


def test_tier3_bare_hex_reprefixes_and_resolves():
    # AC4-HP: 8 lowercase hex, no ab-, no hyphen -> re-prefix -> exact id.
    m = resolve_node("1234abcd", ENTRIES)
    assert m.kind == "exact"
    assert m.id == "ab-1234abcd"


def test_tier3_bare_hex_no_such_node_is_none():
    m = resolve_node("99999999", ENTRIES)
    assert m.kind == "none"
    assert "ab-99999999" in m.note


def test_tier3_not_exactly_8_hex_is_not_bare_hex():
    # AC4-ERR: 10 hex chars is NOT a bare-hex id; resolve_node returns none so
    # the caller treats it as describe-it free text, never a malformed id.
    m = resolve_node("1234abcdef", ENTRIES)
    assert m.kind == "none"


def test_tier3_uppercase_hex_is_not_bare_hex():
    m = resolve_node("1234ABCD", ENTRIES)
    assert m.kind == "none"


# -- resolve_node: tier 3 config-aware bare-hex re-prefix (x-408f) ------------


def test_tier3_config_prefix_resolves(monkeypatch):
    # AC1: on a repo configured with a non-legacy prefix, a bare hex re-prefixes
    # with the configured prefix, not a hardcoded ab-.
    monkeypatch.setattr("fno.graph._constants.node_id_prefix", lambda: "x-")
    entries = [_node("x-4af4", "liveness", slug="liveness")]
    m = resolve_node("4af4", entries)
    assert m.kind == "exact"
    assert m.id == "x-4af4"


def test_tier3_legacy_ab_still_resolves_under_configured_prefix(monkeypatch):
    # AC2 back-compat: even when the repo is configured (x-), a legacy ab-<8hex>
    # id still resolves via the ab- fallback (mixed-format graphs).
    monkeypatch.setattr("fno.graph._constants.node_id_prefix", lambda: "x-")
    m = resolve_node("994222ee", ENTRIES)
    assert m.kind == "exact"
    assert m.id == "ab-994222ee"


def test_tier3_no_match_names_candidates_tried(monkeypatch):
    # AC3: a bare hex matching no key under either prefix returns none, naming
    # both candidates it tried.
    monkeypatch.setattr("fno.graph._constants.node_id_prefix", lambda: "x-")
    m = resolve_node("deadbeef", [_node("x-0000", "nope")])
    assert m.kind == "none"
    assert "x-deadbeef" in m.note
    assert "ab-deadbeef" in m.note


def test_tier3_ambiguous_prefers_configured_prefix(monkeypatch):
    # Boundary: a bare hex that is an exact key under BOTH the configured prefix
    # and ab- resolves to the configured one deterministically (never a silent
    # legacy pick).
    monkeypatch.setattr("fno.graph._constants.node_id_prefix", lambda: "x-")
    entries = [_node("ab-4af4", "legacy"), _node("x-4af4", "configured")]
    m = resolve_node("4af4", entries)
    assert m.kind == "exact"
    assert m.id == "x-4af4"


def test_tier3_config_load_failure_fails_open_to_legacy(monkeypatch):
    # AC4: if load_settings() raises, node_id_prefix() fails open to ab- and the
    # legacy bare-hex path still resolves (no crash).
    def _boom():
        raise RuntimeError("malformed config")
    monkeypatch.setattr("fno.config.load_settings", _boom)
    m = resolve_node("994222ee", ENTRIES)
    assert m.kind == "exact"
    assert m.id == "ab-994222ee"


# -- resolve_node: boundaries ------------------------------------------------


def test_empty_query_is_none():
    assert resolve_node("", ENTRIES).kind == "none"
    assert resolve_node(None, ENTRIES).kind == "none"


def test_exact_id_precedes_slug():
    # A node whose id is checked before any slug: tier 1 wins.
    m = resolve_node("ab-1234abcd", ENTRIES)
    assert m.kind == "exact"
    assert m.id == "ab-1234abcd"


# -- search_entries: high-recall describe-it candidate set -------------------


def test_search_matches_title_token():
    out = search_entries("billing", ENTRIES)
    assert [e["id"] for e in out] == ["ab-1234abcd"]


def test_search_matches_slug_token():
    out = search_entries("dashless-spawn", ENTRIES)
    assert out[0]["id"] == "ab-994222ee"


def test_search_matches_details_token():
    # AC2-HP recall: "iOS autocorrect" lives only in details, not the title.
    out = search_entries("ios autocorrect", ENTRIES)
    assert [e["id"] for e in out] == ["ab-994222ee"]


def test_search_no_tokens_returns_empty():
    assert search_entries("", ENTRIES) == []
    assert search_entries("   ", ENTRIES) == []


def test_search_zero_matches_returns_empty():
    # AC2-ERR: a description matching nothing returns an empty candidate set.
    assert search_entries("quantum teleporter", ENTRIES) == []


def test_search_orders_non_done_before_done():
    entries = [
        _node("ab-dddddddd", "refactor the thing", slug="a", status="done"),
        _node("ab-eeeeeeee", "refactor the other", slug="b", status="ready"),
    ]
    out = search_entries("refactor", entries)
    assert [e["id"] for e in out] == ["ab-eeeeeeee", "ab-dddddddd"]


def test_search_missing_details_is_not_an_error():
    # A node with no details field still matches on title (missing details is
    # not an error, just less to match against).
    entries = [_node("ab-ffffffff", "Standalone", slug="standalone")]
    assert search_entries("standalone", entries)[0]["id"] == "ab-ffffffff"
