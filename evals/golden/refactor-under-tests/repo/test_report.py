"""Tests for report.py - cover build_report behaviour."""
from report import build_report


def test_build_report_single_row():
    result = build_report("apples,3")
    assert "apples: 3" in result
    assert "Total: 3" in result


def test_build_report_multiple_rows():
    result = build_report("apples,3\nbananas,5\ncherries,2")
    assert "apples: 3" in result
    assert "bananas: 5" in result
    assert "cherries: 2" in result
    assert "Total: 10" in result


def test_build_report_empty_lines_ignored():
    result = build_report("apples,3\n\nbananas,5\n")
    assert "Total: 8" in result


def test_build_report_zero_count():
    result = build_report("nothing,0")
    assert "nothing: 0" in result
    assert "Total: 0" in result
