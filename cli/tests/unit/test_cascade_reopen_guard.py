"""A deliberate reopen outranks the automatic close that would undo it.

Both close paths - `_cascade_close_parents` on a child close and
`_sweep_close_done_epics` on reconcile - ask only whether every child carries
`completed_at`. That predicate stays true forever once the last child merges,
so a parent reopened afterwards was re-closed by the next sweep and the reopen
could never hold. Measured 2026-09-05: a node reopened with a written reason
was re-closed by reconcile seven minutes later.

`reopen` requires `--reason` because a close is evidenced by a merged PR while
a reopen is nothing but human judgment. An automatic sweep discarding that
judgment without a word is what these tests pin shut.

Every guard test here has a positive control asserting the SAME fixture closes
once the reopen is removed. Without it a green run cannot tell "the guard
works" from "this fixture never closed anyway".
"""
from __future__ import annotations

from fno.graph.cli import (
    _cascade_close_parents,
    _reopen_outranks_child_closes,
    _strandable_epic_ids,
)

CHILD_CLOSE = "2026-09-04T20:35:52+00:00"
BEFORE_CLOSE = "2026-09-04T19:00:00+00:00"
AFTER_CLOSE = "2026-09-05T04:17:40+00:00"


def _pair(*, reopened_at=None, child_closed=CHILD_CLOSE, parent_closed=None):
    """A parent with one done child, matching the shape that misfired."""
    parent = {"id": "p", "status": "ready"}
    if reopened_at is not None:
        parent["reopened_at"] = reopened_at
    if parent_closed is not None:
        parent["completed_at"] = parent_closed
    child = {"id": "c", "parent": "p", "completed_at": child_closed}
    return parent, child


def test_no_reopen_is_unaffected():
    parent, child = _pair()
    assert _reopen_outranks_child_closes(parent, [child]) is False


def test_reopen_after_every_child_close_holds():
    parent, child = _pair(reopened_at=AFTER_CLOSE)
    assert _reopen_outranks_child_closes(parent, [child]) is True


def test_reopen_before_the_last_child_close_is_stale():
    """It expires by itself: the parent genuinely completed after that call."""
    parent, child = _pair(reopened_at=BEFORE_CLOSE)
    assert _reopen_outranks_child_closes(parent, [child]) is False


def test_reopen_exactly_at_a_child_close_is_stale():
    parent, child = _pair(reopened_at=CHILD_CLOSE)
    assert _reopen_outranks_child_closes(parent, [child]) is False


def test_one_later_child_defeats_an_otherwise_valid_reopen():
    parent, child = _pair(reopened_at=AFTER_CLOSE)
    later = {"id": "c2", "parent": "p", "completed_at": "2026-09-05T09:00:00+00:00"}
    assert _reopen_outranks_child_closes(parent, [child, later]) is False


def test_unreadable_reopen_stamp_protects():
    """Ambiguity favours the human; see the helper's docstring."""
    parent, child = _pair(reopened_at="not-a-timestamp")
    assert _reopen_outranks_child_closes(parent, [child]) is True


def test_blank_reopen_stamp_is_no_reopen():
    parent, child = _pair(reopened_at="   ")
    assert _reopen_outranks_child_closes(parent, [child]) is False


def test_unreadable_child_close_does_not_defeat_protection():
    parent, child = _pair(reopened_at=AFTER_CLOSE, child_closed="garbage")
    assert _reopen_outranks_child_closes(parent, [child]) is True


def test_naive_and_z_suffixed_stamps_compare():
    """The stamps in the graph are written both ways; neither may crash."""
    parent, child = _pair(reopened_at="2026-09-05T04:17:40Z", child_closed="2026-09-04T20:35:52")
    assert _reopen_outranks_child_closes(parent, [child]) is True


def test_sweep_skips_a_reopened_parent():
    parent, child = _pair(reopened_at=AFTER_CLOSE)
    assert _strandable_epic_ids([parent, child]) == set()


def test_sweep_positive_control_same_fixture_closes_without_the_reopen():
    """Without this, a green guard test could just be an inert fixture."""
    parent, child = _pair()
    assert _strandable_epic_ids([parent, child]) == {"p"}


def test_cascade_skips_a_reopened_parent():
    parent, child = _pair(reopened_at=AFTER_CLOSE)
    assert _cascade_close_parents([parent, child], "c") == []
    assert parent.get("completed_at") is None


def test_cascade_positive_control_same_fixture_closes_without_the_reopen():
    parent, child = _pair()
    assert _cascade_close_parents([parent, child], "c") == ["p"]
    assert parent.get("completed_at")
