"""Regression pin: _cascade_close_parents trusts completed_at, not status.

Measured across the live graph on 2026-09-05 (2428 entries): zero rows carry
completed_at while holding a non-terminal status, so the cascade's
completed_at-only guard (graph/cli.py, _cascade_close_parents) never closed a
wrong ancestor. That measurement could go stale silently - this test proves
the detector actually catches a divergence rather than only ever reporting
none.

Filter: ``fno doctor test cli/tests/unit/test_completed_at_status_agreement.py``
"""
from __future__ import annotations

from fno.graph.statuses import completed_at_status_divergence


def _n(node_id: str, status: str, completed_at: str | None) -> dict:
    return {"id": node_id, "status": status, "completed_at": completed_at}


def test_a_consistent_fixture_has_no_divergence():
    entries = [
        _n("ab-1", "done", "2026-08-01T00:00:00+00:00"),
        _n("ab-2", "superseded", "2026-08-02T00:00:00+00:00"),
        _n("ab-3", "in_progress", None),
        _n("ab-4", "idea", None),
    ]
    assert completed_at_status_divergence(entries) == []


def test_the_detector_names_a_real_divergence():
    entries = [
        _n("ab-1", "done", "2026-08-01T00:00:00+00:00"),
        # Constructed exactly the shape the plan's hypothesis predicted and
        # measurement did not find today: completed_at set, status open.
        _n("ab-corrupt", "in_progress", "2026-08-03T00:00:00+00:00"),
    ]
    assert completed_at_status_divergence(entries) == ["ab-corrupt"]


def test_a_non_dict_or_id_less_entry_is_ignored_not_crashed():
    entries = [None, {"status": "in_progress", "completed_at": "x"}, _n("ab-1", "done", "t")]
    assert completed_at_status_divergence(entries) == []
