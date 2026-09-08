"""The one-time pin clear picks the right rows and can be undone (AC6)."""
from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

SCRIPT = (
    Path(__file__).resolve().parents[3]
    / "scripts"
    / "maintenance"
    / "clear-agent-rank-pins.py"
)


@pytest.fixture(scope="module")
def script():
    spec = importlib.util.spec_from_file_location("clear_agent_rank_pins", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_pinned_selects_every_finite_rank(script):
    """Terminal and deferred rows included: a pin left there comes back."""
    entries = [
        {"id": "x-aaa1", "rank": -3.0},
        {"id": "x-ddd4", "rank": -1.0, "completed_at": "2026-09-01T00:00:00+00:00"},
        {"id": "x-eee5", "rank": -2.0, "deferred_at": "2026-09-01T00:00:00+00:00"},
        {"id": "x-bbb2", "rank": None},
        {"id": "x-ccc3"},
        {"id": "x-fff6", "rank": True},
        {"id": "x-999a", "rank": float("nan")},
        {"id": "x-999b", "rank": float("inf")},
    ]

    assert script._pinned(entries) == [("x-aaa1", -3.0), ("x-ddd4", -1.0), ("x-eee5", -2.0)]


def test_clear_then_restore_round_trips(script):
    entries = [{"id": "x-aaa1", "rank": -3.0}, {"id": "x-bbb2", "rank": -1.0}]

    mutator, skipped = script._clear({"x-aaa1": -3.0})
    mutator(entries)
    assert [e.get("rank") for e in entries] == [None, -1.0]
    assert skipped == []

    script._restore({"x-aaa1": -3.0})(entries)
    assert [e.get("rank") for e in entries] == [-3.0, -1.0]


def test_a_rank_written_after_the_preview_is_left_alone(script):
    """The restore file cannot recover a value it never recorded."""
    entries = [{"id": "x-aaa1", "rank": -99.0}]

    mutator, skipped = script._clear({"x-aaa1": -3.0})
    mutator(entries)

    assert entries[0]["rank"] == -99.0
    assert skipped == ["x-aaa1"]
