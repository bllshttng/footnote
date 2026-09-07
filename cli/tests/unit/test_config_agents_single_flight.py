"""The three sweep knobs: their defaults, and what a bad value does.

The degrade is the point. Raising on a typo would fail ``load_settings()`` for
the whole process, so one bad character would make every ``fno`` command exit.
Degrading silently is the other failure: a value nobody can see reads exactly
like a value nobody set. So the contract is degrade AND name it.
"""
from __future__ import annotations

from fno.config import AgentsBlock
from fno.config._sweeps import DEGRADED


def test_defaults_match_the_rust_daemon():
    block = AgentsBlock()
    assert block.single_flight_ttl_seconds == 10
    assert block.single_flight_join_budget_seconds == 30
    assert block.orphan_reap_after_seconds == 5400


def test_configured_values_are_honored():
    block = AgentsBlock(
        single_flight_ttl_seconds=5,
        single_flight_join_budget_seconds=60,
        orphan_reap_after_seconds=900,
    )
    assert block.single_flight_ttl_seconds == 5
    assert block.single_flight_join_budget_seconds == 60
    assert block.orphan_reap_after_seconds == 900


def test_a_non_numeric_value_degrades_and_is_named():
    DEGRADED.clear()
    block = AgentsBlock(single_flight_ttl_seconds="banana")
    assert block.single_flight_ttl_seconds == 10
    assert DEGRADED["agents.single_flight_ttl_seconds"] == "'banana'"


def test_a_non_positive_value_degrades_and_is_named():
    # Zero is not a smaller TTL, it is "never fresh"; zero seconds is not a
    # tighter reap threshold, it is "reap everything". Neither is somewhere a
    # typo should be able to land.
    DEGRADED.clear()
    block = AgentsBlock(orphan_reap_after_seconds=0, single_flight_join_budget_seconds=-3)
    assert block.orphan_reap_after_seconds == 5400
    assert block.single_flight_join_budget_seconds == 30
    assert set(DEGRADED) == {
        "agents.orphan_reap_after_seconds",
        "agents.single_flight_join_budget_seconds",
    }


def test_the_reap_receipt_block_still_rides_the_agents_block():
    # It moved modules with the sweep keys; the config PATH must not have.
    assert AgentsBlock().reap_receipts.retain_days == 7
