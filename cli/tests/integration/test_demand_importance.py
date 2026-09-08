"""The importance projection: evidence first, age as the tiebreak (AC3)."""
from __future__ import annotations

from datetime import datetime, timedelta, timezone

from fno.graph.demand import _AGE_CAP_DAYS, divergence_score, importance_score

NOW = datetime(2026, 9, 7, tzinfo=timezone.utc)


def _voted(votes: int, **over) -> dict:
    entry = {
        "id": "x-aaa1",
        "encounters": [
            {"voter_key": f"voter-{i}", "voter_kind": "agent"} for i in range(votes)
        ],
        "sessions": [{"session_id": "s1"}],
    }
    entry.update(over)
    return entry


def _days_ago(days: int) -> str:
    return (NOW - timedelta(days=days)).isoformat()


def test_no_encounter_scores_zero():
    """Age alone never moves a row: a score with no vote behind it is noise."""
    entry = {"id": "x-aaa1", "touched_at": _days_ago(400), "sessions": [{"s": 1}]}

    assert importance_score(entry, "p2", NOW) == 0.0


def test_score_is_divergence_plus_age():
    entry = _voted(2, touched_at=_days_ago(30))

    assert importance_score(entry, "p2", NOW) == divergence_score(entry, "p2") + 0.3


def test_age_can_never_buy_a_vote():
    """The oldest possible p0 row still loses to one more vote on a fresh one."""
    fresh_loud = _voted(2, touched_at=_days_ago(0))
    ancient_quiet = _voted(1, touched_at=_days_ago(_AGE_CAP_DAYS * 10))

    assert importance_score(fresh_loud, "p0", NOW) > importance_score(
        ancient_quiet, "p0", NOW
    )
    assert importance_score(fresh_loud, "p2", NOW) > importance_score(
        ancient_quiet, "p2", NOW
    )


def test_age_term_is_capped():
    old = _voted(1, touched_at=_days_ago(_AGE_CAP_DAYS))
    older = _voted(1, touched_at=_days_ago(_AGE_CAP_DAYS * 5))

    assert importance_score(old, "p2", NOW) == importance_score(older, "p2", NOW)


def test_touched_at_wins_over_created_at():
    """A deliberate curation touch resets the clock, as maintain reads it."""
    entry = _voted(1, created_at=_days_ago(300), touched_at=_days_ago(0))

    assert importance_score(entry, "p2", NOW) == float(divergence_score(entry, "p2"))


def test_unparseable_stamp_is_no_age_signal():
    entry = _voted(1, touched_at="not-a-date")

    assert importance_score(entry, "p2", NOW) == float(divergence_score(entry, "p2"))


def test_lower_priority_earns_more_divergence():
    """A p3 the fleet keeps hitting is the loudest row the operator is missing."""
    entry = _voted(1, touched_at=_days_ago(0))

    assert importance_score(entry, "p3", NOW) > importance_score(entry, "p0", NOW)
