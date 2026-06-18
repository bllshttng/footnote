"""Unit tests for fno.graph._intake._find_node, including the
prefix-aware wrapper added by ab-7651a2c6 (fuzzy resolver).

The wrapper routes partial ab- IDs (less than 11 chars total: 'ab-' + <8 hex)
through resolve_id so callers like `fno backlog done ab-9728` work without
the user having to type the full 8 hex chars. Full ab-XXXXXXXX queries keep
the fast equality path; non-ab inputs are unchanged.
"""
from __future__ import annotations

from fno.graph._intake import _find_node


def _entry(id: str, title: str = "T", *, status: str = "ready") -> dict:
    return {"id": id, "title": title, "_status": status}


# -- regression: full-id behavior must not change --


def test_find_node_full_id_match_returns_entry():
    entries = [_entry("ab-9728b70b"), _entry("ab-aaaa0001")]
    assert _find_node(entries, "ab-9728b70b") == entries[0]


def test_find_node_full_id_no_match_returns_none():
    entries = [_entry("ab-9728b70b")]
    assert _find_node(entries, "ab-9728b70c") is None


def test_find_node_non_ab_input_unchanged():
    """Non-ab inputs (e.g. raw IDs from older code) keep equality semantics."""
    entries = [{"id": "legacy-001"}, {"id": "abc"}]
    assert _find_node(entries, "legacy-001") == entries[0]
    assert _find_node(entries, "missing") is None


# -- new: prefix-aware lookup for partial ab- IDs --


def test_find_node_prefix_unique_match_returns_entry():
    entries = [
        _entry("ab-9728b70b", "Failover"),
        _entry("ab-aaaa0001", "Other"),
    ]
    assert _find_node(entries, "ab-9728") == entries[0]


def test_find_node_prefix_ambiguous_returns_none():
    """Ambiguity returns None (matches today's 'not found' contract).

    The plan is explicit: callers print their own 'no such node' error, which
    is at least actionable. Surface ambiguity explicitly is a future v1.
    """
    entries = [
        _entry("ab-abcd1111"),
        _entry("ab-abcd2222"),
    ]
    assert _find_node(entries, "ab-abcd") is None


def test_find_node_prefix_ambiguous_writes_stderr(capsys):
    """Ambiguous prefix should name the candidate IDs on stderr so the user
    can disambiguate, rather than seeing only the caller's 'no such node'.
    """
    entries = [
        _entry("ab-abcd1111"),
        _entry("ab-abcd2222"),
    ]
    result = _find_node(entries, "ab-abcd")
    assert result is None
    captured = capsys.readouterr()
    assert "ambiguous" in captured.err
    assert "ab-abcd1111" in captured.err
    assert "ab-abcd2222" in captured.err


def test_find_node_prefix_no_match_returns_none():
    entries = [_entry("ab-9728b70b")]
    assert _find_node(entries, "ab-zzzz") is None


def test_find_node_prefix_too_short_returns_none():
    """ab-XX (below the 4-char prefix floor) falls through and returns None."""
    entries = [_entry("ab-9728b70b")]
    assert _find_node(entries, "ab-97") is None
