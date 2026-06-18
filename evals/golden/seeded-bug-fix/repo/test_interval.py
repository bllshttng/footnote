"""Tests for interval.py."""
from interval import clamp, overlaps


def test_clamp_below_lo():
    assert clamp(0, 5, 10) == 5


def test_clamp_above_hi():
    assert clamp(20, 5, 10) == 10


def test_clamp_inside():
    assert clamp(7, 5, 10) == 7


def test_clamp_at_lo():
    assert clamp(5, 5, 10) == 5


def test_clamp_at_hi():
    # This test FAILS against the seeded bug (clamp(10,5,10) returns 9, not 10)
    assert clamp(10, 5, 10) == 10


def test_overlaps_yes():
    assert overlaps(1, 5, 3, 8) is True


def test_overlaps_no():
    assert overlaps(1, 3, 5, 8) is False


def test_overlaps_touching():
    assert overlaps(1, 5, 5, 8) is True
