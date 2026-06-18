"""Tests for fno.reality_check.{notion,sheets} - structured not-implemented stubs."""
from __future__ import annotations

import json
from pathlib import Path

import pytest


# -- AC1-HP: notion stub exits 0 with not-implemented marker --

@pytest.mark.skip_when_implemented("notion")
def test_ac1_hp_notion_stub_returns_not_implemented() -> None:
    """AC1-HP: check_notion returns ok:false with kind:not-implemented, domain:notion."""
    from fno.reality_check.notion import check_notion

    result = check_notion()

    assert result["ok"] is False
    assert result["error"]["kind"] == "not-implemented"
    assert result["error"]["domain"] == "notion"


@pytest.mark.skip_when_implemented("notion")
def test_ac1_hp_notion_stub_accepts_any_kwargs() -> None:
    """AC1-HP: check_notion accepts arbitrary keyword args without crashing."""
    from fno.reality_check.notion import check_notion

    result = check_notion(target="some-page", database_id="abc123", extra_arg="ignored")

    assert result["ok"] is False
    assert result["error"]["kind"] == "not-implemented"


# -- AC1-HP: sheets stub exits 0 with not-implemented marker --

@pytest.mark.skip_when_implemented("sheets")
def test_ac1_hp_sheets_stub_returns_not_implemented() -> None:
    """AC1-HP: check_sheets returns ok:false with kind:not-implemented, domain:sheets."""
    from fno.reality_check.sheets import check_sheets

    result = check_sheets()

    assert result["ok"] is False
    assert result["error"]["kind"] == "not-implemented"
    assert result["error"]["domain"] == "sheets"


@pytest.mark.skip_when_implemented("sheets")
def test_ac1_hp_sheets_stub_accepts_any_kwargs() -> None:
    """AC1-HP: check_sheets accepts arbitrary keyword args without crashing."""
    from fno.reality_check.sheets import check_sheets

    result = check_sheets(spreadsheet_id="xyz", range="A1:B2", extra="ignored")

    assert result["ok"] is False
    assert result["error"]["kind"] == "not-implemented"


# -- Structural: result is JSON-serializable --

def test_notion_result_is_json_serializable() -> None:
    """Structural: check_notion result can be json.dumps'd without error."""
    from fno.reality_check.notion import check_notion

    result = check_notion()
    serialized = json.dumps(result)
    parsed = json.loads(serialized)
    assert parsed == result


def test_sheets_result_is_json_serializable() -> None:
    """Structural: check_sheets result can be json.dumps'd without error."""
    from fno.reality_check.sheets import check_sheets

    result = check_sheets()
    serialized = json.dumps(result)
    parsed = json.loads(serialized)
    assert parsed == result


# -- Callers get structured "not yet", not an exception --

def test_notion_does_not_raise() -> None:
    """Stub must not raise any exception - callers depend on a structured response."""
    from fno.reality_check.notion import check_notion

    try:
        result = check_notion(anything="whatever")
    except Exception as exc:
        pytest.fail(f"check_notion raised unexpectedly: {exc}")

    assert isinstance(result, dict)


def test_sheets_does_not_raise() -> None:
    """Stub must not raise any exception - callers depend on a structured response."""
    from fno.reality_check.sheets import check_sheets

    try:
        result = check_sheets(anything="whatever")
    except Exception as exc:
        pytest.fail(f"check_sheets raised unexpectedly: {exc}")

    assert isinstance(result, dict)
