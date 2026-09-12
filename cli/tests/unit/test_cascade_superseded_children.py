"""A parent closes when its superseded children were replaced elsewhere.

``supersede`` never stamps ``completed_at``, so the old all-children-carry-
``completed_at`` read kept any parent holding a superseded child open
forever. The replacement, ``children_all_closed``, asks three things: every
child terminal, no child superseded BY the parent itself (a child the parent
absorbed means the parent's own work is still open), and at least one child
that really shipped. Each shape runs against both close paths:
``_cascade_close_parents`` (child-close event) and ``_strandable_epic_ids``
(reconcile sweep).

Every stays-open case has a positive control asserting the SAME fixture
closes once the blocking term is removed, so a green run cannot mean "this
fixture never closed anyway".

Filter: ``fno doctor test cli/tests/unit/test_cascade_superseded_children.py``
"""
from __future__ import annotations

from fno.graph.cli import _cascade_close_parents, _strandable_epic_ids

CHILD_CLOSE = "2026-09-04T20:35:52+00:00"
AFTER_CLOSE = "2026-09-05T04:17:40+00:00"


def _world(*, status=None, superseded_by=None, reopened_at=None):
    """A parent with one done child and one caller-shaped second child."""
    parent = {"id": "p", "status": "ready"}
    if reopened_at is not None:
        parent["reopened_at"] = reopened_at
    done = {"id": "c1", "parent": "p", "completed_at": CHILD_CLOSE}
    second = {"id": "c2", "parent": "p"}
    if status is not None:
        second["status"] = status
    if superseded_by is not None:
        second["superseded_by"] = superseded_by
    return parent, done, second


# --- replaced elsewhere: terminal child pointing at a sibling -------------


def test_cascade_closes_when_the_only_other_child_was_replaced_elsewhere():
    parent, done, second = _world(status="superseded", superseded_by="other")
    assert _cascade_close_parents([parent, done, second], "c1") == ["p"]
    assert parent.get("completed_at")


def test_sweep_names_the_parent_when_the_only_other_child_was_replaced_elsewhere():
    parent, done, second = _world(status="superseded", superseded_by="other")
    assert _strandable_epic_ids([parent, done, second]) == {"p"}


def test_cascade_positive_control_the_same_child_open_blocks():
    parent, done, second = _world(status="ready")
    assert _cascade_close_parents([parent, done, second], "c1") == []
    assert parent.get("completed_at") is None


# --- absorbed by the parent: the child points BACK at this parent ---------


def test_absorbed_child_keeps_the_parent_open_on_the_cascade():
    parent, done, second = _world(status="superseded", superseded_by="p")
    assert _cascade_close_parents([parent, done, second], "c1") == []
    assert parent.get("completed_at") is None


def test_absorbed_child_keeps_the_parent_open_on_the_sweep():
    parent, done, second = _world(status="superseded", superseded_by="p")
    assert _strandable_epic_ids([parent, done, second]) == set()


def test_absorbed_positive_control_the_same_child_replaced_elsewhere_closes():
    parent, done, second = _world(status="superseded", superseded_by="other")
    assert _strandable_epic_ids([parent, done, second]) == {"p"}


# --- nothing shipped: every child replaced, nothing ever completed --------


def test_a_parent_whose_every_child_was_replaced_elsewhere_stays_open():
    parent, done, second = _world(status="superseded", superseded_by="other")
    done["status"] = "superseded"
    done["superseded_by"] = "other2"
    done.pop("completed_at")
    assert _cascade_close_parents([parent, done, second], "c1") == []
    assert _strandable_epic_ids([parent, done, second]) == set()


def test_nothing_shipped_positive_control_one_real_close_frees_the_parent():
    parent, done, second = _world(status="superseded", superseded_by="other")
    assert _cascade_close_parents([parent, done, second], "c1") == ["p"]


# --- reopen still outranks the sweep on the new predicate -----------------


def test_a_reopen_still_holds_even_when_the_rest_of_the_children_closed():
    parent, done, second = _world(
        status="superseded", superseded_by="other", reopened_at=AFTER_CLOSE
    )
    assert _cascade_close_parents([parent, done, second], "c1") == []
    assert _strandable_epic_ids([parent, done, second]) == set()


def test_reopen_positive_control_the_same_world_closes_without_the_reopen():
    parent, done, second = _world(status="superseded", superseded_by="other")
    assert _cascade_close_parents([parent, done, second], "c1") == ["p"]
