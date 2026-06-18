"""Tests for duration.py - spec tests that drive the implementation."""
import pytest
from duration import parse_duration


def test_parse_seconds():
    assert parse_duration("90s") == 90


def test_parse_minutes():
    assert parse_duration("5m") == 300


def test_parse_hours_and_minutes():
    assert parse_duration("1h30m") == 5400


def test_parse_zero():
    assert parse_duration("0s") == 0


def test_parse_empty_raises():
    with pytest.raises(ValueError):
        parse_duration("")


def test_parse_garbage_raises():
    with pytest.raises(ValueError):
        parse_duration("abc")


def test_parse_negative_raises():
    with pytest.raises(ValueError):
        parse_duration("-5s")
