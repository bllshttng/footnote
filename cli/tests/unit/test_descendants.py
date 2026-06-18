"""Unit tests for the transitive-children resolver (C2, ab-facfaade).

`descendants_of` powers the `--parent <epic-id>` epic-scope filter on
`fno backlog next`/`ready` and the epic-subtree megawalk. It returns the
set of node IDs that are transitive children of an epic via the `parent`
field, cycle-safe against a malformed graph.
"""
from fno.graph._intake import descendants_of


def test_direct_children():
    entries = [
        {"id": "epic"},
        {"id": "c1", "parent": "epic"},
        {"id": "c2", "parent": "epic"},
        {"id": "loose"},
    ]
    assert descendants_of(entries, "epic") == {"c1", "c2"}


def test_transitive_grandchildren():
    entries = [
        {"id": "epic"},
        {"id": "c1", "parent": "epic"},
        {"id": "g1", "parent": "c1"},
        {"id": "g2", "parent": "c1"},
    ]
    assert descendants_of(entries, "epic") == {"c1", "g1", "g2"}


def test_excludes_self():
    entries = [{"id": "epic"}, {"id": "c1", "parent": "epic"}]
    assert "epic" not in descendants_of(entries, "epic")


def test_no_children_returns_empty():
    entries = [{"id": "epic"}, {"id": "loose"}]
    assert descendants_of(entries, "epic") == set()


def test_unknown_parent_returns_empty():
    entries = [{"id": "a"}, {"id": "b", "parent": "a"}]
    assert descendants_of(entries, "nonexistent") == set()


def test_cycle_safe():
    # A malformed parent cycle must not hang or recurse forever.
    entries = [
        {"id": "epic"},
        {"id": "c1", "parent": "epic"},
        {"id": "c2", "parent": "c1"},
        {"id": "c1again", "parent": "c2"},
        # Inject a cycle: c1's parent flips to one of its own descendants.
        {"id": "loop", "parent": "loop"},
    ]
    result = descendants_of(entries, "epic")
    assert result == {"c1", "c2", "c1again"}


def test_malformed_entries_tolerated():
    entries = [
        {"id": "epic"},
        "not-a-dict",
        {"no_id": True},
        {"id": "c1", "parent": "epic"},
    ]
    assert descendants_of(entries, "epic") == {"c1"}


def test_cycle_back_to_parent_excludes_parent():
    # A malformed chain that loops back to the scoped parent must not add
    # the parent to its own descendant set (contract: parent excluded).
    entries = [
        {"id": "epic", "parent": "c2"},   # cycle: epic's parent is its own grandchild
        {"id": "c1", "parent": "epic"},
        {"id": "c2", "parent": "c1"},
    ]
    result = descendants_of(entries, "epic")
    assert "epic" not in result
    assert result == {"c1", "c2"}


def test_unhashable_parent_does_not_crash():
    # A corrupted row whose `parent` is a dict/list must be skipped, not
    # raise TypeError on the children-index insert.
    entries = [
        {"id": "epic"},
        {"id": "bad", "parent": {"nested": "dict"}},
        {"id": "bad2", "parent": ["list"]},
        {"id": "c1", "parent": "epic"},
    ]
    assert descendants_of(entries, "epic") == {"c1"}
