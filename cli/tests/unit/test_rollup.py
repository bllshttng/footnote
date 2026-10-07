"""Rollup visibility: the orphan predicate and the ancestor walk it leans on.

The filing-time ladder (resolve, receipts, role override) is native now; its
behavior is pinned by the autolink unit tests and the create-surface golden
replays beside the Rust owner.
"""
from __future__ import annotations

import pytest

from fno.graph.rollup import (
    has_epic_ancestor,
    is_orphan,
    orphan_ids,
)


def node(nid, **kw):
    base = {"id": nid, "type": "feature", "title": nid, "details": ""}
    base.update(kw)
    return base


def epic(nid, title, **kw):
    return node(nid, type="epic", title=title, **kw)


# -- orphan predicate --


def test_feature_with_no_parent_is_orphan():
    entries = [node("x-1")]
    assert orphan_ids(entries) == {"x-1"}


def test_feature_parented_to_epic_is_not_orphan():
    entries = [epic("x-e", "mux polish"), node("x-1", parent="x-e")]
    assert orphan_ids(entries) == frozenset()


def test_epic_ancestor_found_through_intermediate_node():
    """A leaf under a group child under an epic still has a mission edge."""
    entries = [
        epic("x-e", "mux polish"),
        node("x-mid", parent="x-e"),
        node("x-leaf", parent="x-mid"),
    ]
    assert orphan_ids(entries) == frozenset()


def test_parent_pointing_at_missing_node_is_orphan():
    entries = [node("x-1", parent="x-gone")]
    assert orphan_ids(entries) == {"x-1"}


def test_parent_cycle_terminates_and_reports_orphan():
    entries = [node("x-a", parent="x-b"), node("x-b", parent="x-a")]
    assert orphan_ids(entries) == {"x-a", "x-b"}


@pytest.mark.parametrize("type_", ["bug", "epic", "roadmap"])
def test_non_rollup_types_are_never_orphans(type_):
    """AC6: bugs are exempt by type; so are containers."""
    entries = [node("x-1", type=type_)]
    assert orphan_ids(entries) == frozenset()


def test_orphan_ok_exempts_the_node():
    """AC6: a deliberate orphan is invisible to the predicate."""
    entries = [node("x-1", orphan_ok="infra")]
    assert orphan_ids(entries) == frozenset()


def test_empty_orphan_ok_does_not_exempt():
    """An empty reason is not an opt-out - it is an unset field."""
    entries = [node("x-1", orphan_ok="")]
    assert orphan_ids(entries) == {"x-1"}


def test_has_epic_ancestor_on_malformed_entries():
    assert has_epic_ancestor({"parent": None}, {}) is False
    assert has_epic_ancestor({}, {}) is False


def test_is_orphan_tolerates_non_dict():
    assert is_orphan("not-a-node", {}) is False  # type: ignore[arg-type]


# -- epic candidate scoring --
