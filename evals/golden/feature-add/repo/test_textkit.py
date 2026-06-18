"""Tests for textkit.py."""
from textkit import reverse, truncate


def test_reverse_basic():
    assert reverse("hello") == "olleh"


def test_reverse_empty():
    assert reverse("") == ""


def test_truncate_shorter():
    assert truncate("hi", 10) == "hi"


def test_truncate_exact():
    assert truncate("hello", 5) == "hello"


def test_truncate_longer():
    assert truncate("hello world", 5) == "hello"
