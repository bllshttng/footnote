"""`fno carveout resolve --reason` records why a row was retired without a node.

The emit is best-effort by design (the rows are already gone), so a broken
payload cannot fail the command. That is exactly why it needs a test: the first
real use emitted an invalid envelope source and the reason was lost, with the
ledger already mutated. This pins the payload against the live schema.
"""
from __future__ import annotations

import pytest

from fno.events import _build


def test_carveout_resolved_validates_against_the_live_schema():
    event = _build(
        "carveout_resolved",
        "backlog",
        {
            "carveout_ids": "cv-11112222,cv-33334444",
            "reason": "test residue; no work here",
            "removed": 2,
        },
    )

    assert event["type"] == "carveout_resolved"
    assert event["source"] == "backlog"
    assert event["data"]["reason"]


def test_an_invalid_source_is_rejected_rather_than_written():
    """The positive control for the test above: _build really does validate the
    source, so a passing test means the enum was honored, not skipped."""
    with pytest.raises(Exception):
        _build("carveout_resolved", "carveout", {"carveout_ids": "cv-1", "reason": "x"})


def test_reason_is_required_by_the_schema():
    with pytest.raises(Exception):
        _build("carveout_resolved", "backlog", {"carveout_ids": "cv-1"})
