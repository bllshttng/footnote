"""A voted-for row survives the age drain (AC4)."""
from __future__ import annotations

from datetime import datetime, timedelta, timezone

from fno.graph.maintain import detect_stale_ideas, is_stale_ready, node_has_movement

NOW = datetime(2026, 9, 7, tzinfo=timezone.utc)
STALENESS = 30


def _idea(node_id: str, age_days: int, votes: int = 0) -> dict:
    entry = {
        "id": node_id,
        "status": "idea",
        "priority": "p2",
        "project": "fno",
        "created_at": (NOW - timedelta(days=age_days)).isoformat(),
    }
    if votes:
        entry["encounters"] = [
            {"voter_key": f"{node_id}-voter-{i}", "voter_kind": "agent"}
            for i in range(votes)
        ]
    return entry


def _drained(entries: list[dict]) -> list[str]:
    return [row.node_id for row in detect_stale_ideas(entries, STALENESS, NOW)]


def test_ac4_hp_encountered_idea_survives_the_drain():
    """40 days old with one vote stays; 31 days old with none is drained."""
    voted = _idea("x-aaa1", age_days=40, votes=1)
    silent = _idea("x-bbb2", age_days=31)

    assert _drained([voted, silent]) == ["x-bbb2"]


def test_ac4_hp_encounter_is_movement():
    assert node_has_movement(_idea("x-aaa1", 40, votes=1), NOW, STALENESS) is True
    assert node_has_movement(_idea("x-bbb2", 40), NOW, STALENESS) is False


def test_ac4_encounter_rows_without_a_voter_key_do_not_count():
    """An empty encounters list, or rows with no identity, is no vote."""
    entry = _idea("x-aaa1", 40)
    entry["encounters"] = [{}, {"evidence": "no voter"}]

    assert node_has_movement(entry, NOW, STALENESS) is False
    assert _drained([entry]) == ["x-aaa1"]


def test_ac4_encounter_also_spares_a_stale_ready_node():
    """One movement gate, both callers: the ready quarantine reads it too."""
    ready = {
        "id": "x-ccc3",
        "status": "ready",
        "priority": "p2",
        "project": "fno",
        "plan_path": "plans/x-ccc3.md",
        "created_at": (NOW - timedelta(days=90)).isoformat(),
    }
    assert is_stale_ready(ready, NOW, STALENESS) is True

    ready["encounters"] = [{"voter_key": "v1", "voter_kind": "agent"}]
    assert is_stale_ready(ready, NOW, STALENESS) is False
