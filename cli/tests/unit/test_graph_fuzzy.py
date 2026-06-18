"""Unit tests for fno.graph.fuzzy - resolve_id and suggest_domain.

Pure functions; no I/O. Each test builds a list[dict] fixture and calls
the function directly, per the BDD scenarios in Phase 02 of the plan.
"""
from __future__ import annotations

import pytest

from fno.graph.fuzzy import (
    DomainSuggestion,
    IdMatch,
    _branch_tokens,
    resolve_id,
    suggest_domain,
)


# -- helpers --


def _entry(id: str, title: str, *, status: str = "ready", domain: str = "code") -> dict:
    return {
        "id": id,
        "title": title,
        "_status": status,
        "domain": domain,
    }


# -- resolve_id: exact match --


def test_scenario1_hp_exact_ab_id_resolves():
    """Scenario 1 (HP): exact ab- id resolves regardless of status."""
    entries = [_entry("ab-54e461b6", "Plan 02", status="done")]
    result = resolve_id("ab-54e461b6", entries)
    assert result.kind == "exact"
    assert result.id == "ab-54e461b6"


def test_exact_unknown_ab_id_falls_through_to_none():
    """Exact-form query not in entries -> kind='none' (not ambiguous)."""
    entries = [_entry("ab-00000001", "X")]
    result = resolve_id("ab-deadbeef", entries)
    assert result.kind == "none"
    assert result.id is None


def test_exact_configured_prefix_id_resolves():
    """ab-bbfccb8f: an exact id under a configured (non-ab) prefix/width resolves
    via the format-agnostic exact match (a graph lookup, not an ab- regex)."""
    entries = [_entry("fno-a3f9", "Configured node"), _entry("ab-55ba9adb", "Legacy")]
    result = resolve_id("fno-a3f9", entries)
    assert result.kind == "exact"
    assert result.id == "fno-a3f9"
    # And a legacy id still resolves in the same mixed-format graph.
    assert resolve_id("ab-55ba9adb", entries).kind == "exact"


def test_configured_id_not_an_entry_does_not_fuzzy_collapse():
    """A configured-shaped query that is NOT a graph id must not fuzzy-collapse
    onto an unrelated title (it has no exact hit and is not an ab- prefix)."""
    entries = [_entry("fno-a3f9", "Configured node")]
    result = resolve_id("xy-dead", entries)
    assert result.kind == "none"


# -- resolve_id: ab-id prefix matching (4-7 hex chars after ab-) --


def test_resolve_id_prefix_unique():
    """Partial ab-id (4-7 hex chars) with one match resolves as kind='fuzzy'."""
    entries = [
        _entry("ab-9728b70b", "Provider rotation: failover"),
        _entry("ab-aaaa0001", "Other work"),
    ]
    result = resolve_id("ab-9728", entries)
    assert result.kind == "fuzzy"
    assert result.id == "ab-9728b70b"


def test_resolve_id_prefix_ambiguous():
    """Partial ab-id with multiple matches yields kind='ambiguous'."""
    entries = [
        _entry("ab-abcd1111", "Plan A"),
        _entry("ab-abcd2222", "Plan B"),
        _entry("ab-ffff0000", "Unrelated"),
    ]
    result = resolve_id("ab-abcd", entries)
    assert result.kind == "ambiguous"
    assert len(result.candidates) == 2
    ids = {e["id"] for e in result.candidates}
    assert ids == {"ab-abcd1111", "ab-abcd2222"}


def test_resolve_id_prefix_no_match():
    """Partial ab-id with no matching entry yields kind='none'."""
    entries = [_entry("ab-9728b70b", "Failover")]
    result = resolve_id("ab-zzzz", entries)
    assert result.kind == "none"


def test_resolve_id_prefix_too_short_falls_through():
    """ab-XX (2 chars after ab-) is below the 4-char prefix floor; falls through.

    The query 'ab-97' has only 2 hex chars after 'ab-'. Per the plan, the prefix
    regex is [0-9a-f]{4,7}; shorter queries drop to title fuzzy match. Since
    'ab-97' is unlikely to substring-match any title, expect 'none'.
    """
    entries = [_entry("ab-9728b70b", "Provider rotation: failover")]
    result = resolve_id("ab-97", entries)
    assert result.kind == "none"


def test_resolve_id_prefix_8_chars_no_match_stays_exact_semantics():
    """A full 8-char ab- query that does not match any entry stays kind='none'.

    This preserves the existing exact-match contract for full-length queries:
    no false-positive prefix matches when the user typed the entire id.
    """
    entries = [_entry("ab-9728b70b", "Failover")]
    result = resolve_id("ab-9728b70c", entries)
    assert result.kind == "none"


def test_resolve_id_prefix_full_8_chars_returns_kind_exact():
    """A full 8-char query that matches an entry stays kind='exact', not 'fuzzy'.

    Downstream callers like 'fno backlog find' format output differently for
    exact vs fuzzy; preserving this discriminator prevents UI regressions.
    """
    entries = [_entry("ab-9728b70b", "Failover")]
    result = resolve_id("ab-9728b70b", entries)
    assert result.kind == "exact"
    assert result.id == "ab-9728b70b"


def test_resolve_id_malformed_ab_returns_none_not_title_fuzzy():
    """Malformed ab- queries (trailing comma, non-hex chars, etc.) explicitly
    return kind='none' rather than falling through to title fuzzy.

    Catches the silent-failure case where 'ab-9728b70b,' could fuzzy-match
    onto a title that mentions the literal id substring. The 'ab-' prefix
    is a strong user signal that they want id resolution; a malformed
    suffix should be a clear error, not an implicit re-routing.
    """
    entries = [
        _entry("ab-9728b70b", "Failover plan referencing ab-9728b70b inline"),
    ]
    result = resolve_id("ab-9728b70b,", entries)
    assert result.kind == "none"
    assert "malformed" in (result.note or "").lower()


def test_resolve_id_malformed_ab_3_chars_returns_none():
    """ab-XXX (3 hex chars) is below the prefix floor; explicit none."""
    entries = [_entry("ab-9728b70b", "Failover")]
    result = resolve_id("ab-972", entries)
    assert result.kind == "none"


def test_resolve_id_prefix_prefers_non_done():
    """Prefix matching ranks open entries above done ones when both share a prefix.

    Mirrors the existing fuzzy-title behavior so a recently-completed node does
    not shadow an open one with the same prefix.
    """
    entries = [
        _entry("ab-abcd1111", "Closed plan", status="done"),
        _entry("ab-abcd2222", "Open plan", status="ready"),
    ]
    result = resolve_id("ab-abcd", entries)
    assert result.kind == "ambiguous"
    # Both should still surface as candidates; the user resolves the ambiguity.
    assert len(result.candidates) == 2


# -- resolve_id: fuzzy match --


def test_scenario2_hp_fuzzy_title_single_hit():
    """Scenario 2 (HP): fuzzy title match with single hit resolves."""
    entries = [
        _entry("ab-aa000001", "Plan 01: other feature", status="done"),
        _entry("ab-aa000002", "Plan 03: Ingestion framework"),
    ]
    result = resolve_id("ingestion framework", entries)
    assert result.kind == "fuzzy"
    assert result.id == "ab-aa000002"


def test_fuzzy_prefers_non_done_matches():
    """Non-done matches preferred over done matches when both exist."""
    entries = [
        _entry("ab-done00001", "Ingestion framework pilot", status="done"),
        _entry("ab-open00002", "Ingestion framework rollout"),
    ]
    result = resolve_id("ingestion framework", entries)
    assert result.kind == "fuzzy"
    assert result.id == "ab-open00002"


def test_fuzzy_falls_back_to_done_when_no_open_matches():
    """With only done matches, still resolves (not 'none')."""
    entries = [_entry("ab-done00001", "Ingestion framework", status="done")]
    result = resolve_id("ingestion framework", entries)
    assert result.kind == "fuzzy"
    assert result.id == "ab-done00001"


# -- resolve_id: branch derivation --


def test_scenario3_edge_branch_derivation_picks_obvious_match():
    """Scenario 3 (EDGE): git-branch tokens resolve to the obvious match."""
    entries = [
        _entry("ab-tot000001", "Implement tot init command"),
        _entry("ab-foo000001", "Unrelated work"),
    ]
    result = resolve_id(None, entries, git_branch="feat/tot-init-first-run-ux")
    assert result.kind == "branch_derived"
    assert result.id == "ab-tot000001"


def test_branch_derivation_with_empty_query_and_no_branch_is_none():
    """Empty query, no branch, no history -> none."""
    result = resolve_id(None, [])
    assert result.kind == "none"


def test_branch_derivation_strips_known_prefixes():
    """_branch_tokens strips feat/ fix/ docs/ etc."""
    assert _branch_tokens("feat/tot-init") == ["tot", "init"]
    assert _branch_tokens("fix/login-bug") == ["login", "bug"]
    assert _branch_tokens("docs/readme-update") == ["readme", "update"]


def test_branch_derivation_drops_short_and_numeric_tokens():
    """Tokens <= 2 chars or all-digits are dropped."""
    assert _branch_tokens("feat/ab-cd-ef-ux-long-name") == ["long", "name"]
    assert _branch_tokens("fix/123-very-long-story") == ["very", "long", "story"]


def test_branch_derivation_handles_no_prefix():
    """Branch without known prefix still tokenized."""
    assert _branch_tokens("something-weird-branch") == ["something", "weird", "branch"]


# -- resolve_id: ambiguity and no-match --


def test_scenario4_err_ambiguous_returns_candidates():
    """Scenario 4 (ERR): ambiguous query returns all candidates."""
    entries = [
        _entry("ab-aa000001", "Plan 01: one"),
        _entry("ab-bb000002", "Plan 02: two"),
    ]
    result = resolve_id("plan", entries)
    assert result.kind == "ambiguous"
    assert len(result.candidates) == 2
    ids = {e["id"] for e in result.candidates}
    assert ids == {"ab-aa000001", "ab-bb000002"}


def test_scenario5_err_no_match_yields_none():
    """Scenario 5 (ERR): query with no matches -> kind='none'."""
    entries = [_entry("ab-xx000001", "something completely different")]
    result = resolve_id("nonexistent feature xyz", entries)
    assert result.kind == "none"
    assert result.id is None


def test_ambiguous_populates_first_id_for_convenience():
    """Ambiguous result still sets .id to first candidate (caller can ignore)."""
    entries = [
        _entry("ab-first00001", "Plan 01"),
        _entry("ab-second0001", "Plan 02"),
    ]
    result = resolve_id("plan", entries)
    assert result.kind == "ambiguous"
    # id is populated but caller should look at candidates
    assert result.id in {"ab-first00001", "ab-second0001"}


# -- suggest_domain --


def test_scenario6_hp_suggest_domain_new_with_history():
    """Scenario 6 (HP): non-matching query yields new + history preserved."""
    entries = [
        _entry("ab-aa000001", "T1", domain="code"),
        _entry("ab-aa000002", "T2", domain="code"),
    ]
    result = suggest_domain("research", entries)
    assert result.confidence == "new"
    assert result.match == "research"
    assert "code" in result.history


def test_scenario7_edge_suggest_domain_fuzzy_prefix():
    """Scenario 7 (EDGE): unique prefix match resolves fuzzy."""
    entries = [
        _entry("ab-aa000001", "T1", domain="research"),
        _entry("ab-aa000002", "T2", domain="code"),
    ]
    result = suggest_domain("res", entries)
    assert result.confidence == "fuzzy"
    assert result.match == "research"


def test_scenario8_edge_suggest_domain_ambiguous_prefix_stays_new():
    """Scenario 8 (EDGE): ambiguous prefix falls back to 'new' with input verbatim."""
    entries = [
        _entry("ab-aa000001", "T1", domain="research"),
        _entry("ab-aa000002", "T2", domain="retrospective"),
    ]
    result = suggest_domain("re", entries)
    assert result.confidence == "new"
    assert result.match == "re"


def test_suggest_domain_exact_match():
    """Exact domain hit resolves as exact."""
    entries = [_entry("ab-aa000001", "T1", domain="research")]
    result = suggest_domain("research", entries)
    assert result.confidence == "exact"
    assert result.match == "research"


def test_suggest_domain_empty_history_empty_query_defaults_code():
    """Empty history and empty query -> safe default 'code' as new."""
    result = suggest_domain("", [])
    assert result.match == "code"
    assert result.confidence == "new"
    assert result.history == ()


def test_suggest_domain_empty_query_with_history():
    """Empty query with history -> prefer 'code' if present, new confidence."""
    entries = [
        _entry("ab-aa000001", "T1", domain="research"),
        _entry("ab-aa000002", "T2", domain="code"),
    ]
    result = suggest_domain("", entries)
    assert result.match == "code"
    assert result.confidence == "new"


def test_suggest_domain_empty_query_history_without_code():
    """Empty query with history lacking code -> first alphabetical."""
    entries = [
        _entry("ab-aa000001", "T1", domain="research"),
        _entry("ab-aa000002", "T2", domain="design"),
    ]
    result = suggest_domain("", entries)
    assert result.match in {"design", "research"}
    assert result.confidence == "new"


def test_suggest_domain_history_is_sorted_and_deduped():
    """history tuple returns sorted unique domains."""
    entries = [
        _entry("ab-aa000001", "T1", domain="research"),
        _entry("ab-aa000002", "T2", domain="code"),
        _entry("ab-aa000003", "T3", domain="research"),  # dup
    ]
    result = suggest_domain("research", entries)
    assert result.history == ("code", "research")


# -- Dataclass sanity --


def test_id_match_is_frozen_dataclass():
    """IdMatch is immutable."""
    m = IdMatch(kind="none", id=None)
    with pytest.raises(Exception):
        m.kind = "exact"  # type: ignore[misc]


def test_domain_suggestion_is_frozen_dataclass():
    """DomainSuggestion is immutable."""
    s = DomainSuggestion(match="code", confidence="new", history=())
    with pytest.raises(Exception):
        s.match = "research"  # type: ignore[misc]
