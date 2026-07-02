"""Tests for the legible dispatch branch name helper (x-ff83 W3).

Covers <prefix>/<slug>-<node> construction, empty-slug degradation,
over-long-slug truncation (id preserved), config.branch.prefix, and
round-trip prefix stripping in graph fuzzy resolution.
"""
from __future__ import annotations

from fno.worktree import branch_name


def test_prefix_slug_node(monkeypatch):
    """AC3-HP: <prefix>/<slug>-<node> with the full node id."""
    assert (
        branch_name("x-ff83", slug="plan-docs-consistency", prefix="fno")
        == "fno/plan-docs-consistency-x-ff83"
    )


def test_empty_slug_degrades_to_prefix_node():
    """AC3-ERR: an empty slug never yields a dangling <prefix>/-<node>."""
    assert branch_name("x-ff83", slug="", prefix="fno") == "fno/x-ff83"
    assert branch_name("x-ff83", slug=None, prefix="fno").startswith("fno/")
    assert "//" not in branch_name("x-ff83", slug="", prefix="fno")
    assert "/-" not in branch_name("x-ff83", slug="", prefix="fno")


def test_overlong_slug_truncated_id_intact():
    """AC3-EDGE: a very long slug is truncated; the full node id is preserved."""
    long_slug = "a" * 200
    out = branch_name("x-ff83", slug=long_slug, prefix="fno")
    assert out.endswith("-x-ff83")  # id preserved verbatim
    assert len(out) < 100  # slug component bounded


def test_slug_ref_sanitized():
    """A slug with ref-unsafe chars is dash-normalized (no spaces/~^:?*[)."""
    out = branch_name("x-ff83", slug="Weird Slug: v2*", prefix="fno")
    assert out == "fno/weird-slug-v2-x-ff83"
    for bad in " ~^:?*[\\":
        assert bad not in out


def test_default_prefix_is_fno(monkeypatch):
    """No explicit prefix -> config.branch.prefix (default 'fno')."""
    out = branch_name("x-ff83", slug="s")
    assert out == "fno/s-x-ff83"


def test_config_branch_prefix_default():
    from fno.config import load_settings

    assert load_settings().config.branch.prefix == "fno"


def test_config_branch_prefix_rejects_unsafe():
    import pytest
    from fno.config import BranchBlock

    with pytest.raises(ValueError):
        BranchBlock(prefix="bad prefix")
    with pytest.raises(ValueError):
        BranchBlock(prefix="")


def test_fuzzy_strips_fno_prefix_for_round_trip():
    """The new prefix is stripped like feature/ so branch tokens resolve."""
    from fno.graph.fuzzy import _KNOWN_BRANCH_PREFIXES, _branch_tokens

    assert "fno/" in _KNOWN_BRANCH_PREFIXES
    toks = _branch_tokens("fno/plan-docs-consistency-x-ff83")
    assert "plan" in toks and "docs" in toks
    assert "fno" not in toks  # prefix stripped, not treated as a token
