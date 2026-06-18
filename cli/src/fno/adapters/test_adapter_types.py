"""Tests for AdapterHealth.__post_init__ reason-guard (Task 1.1, AC-ERR).

These tests are written BEFORE the implementation so they start red.
"""
from __future__ import annotations

import pytest

from fno.adapters.base import AdapterHealth


def test_adapter_health_rejects_empty_details_when_not_ok():
    """AC-ERR: AdapterHealth(ok=False, details={}) raises ValueError."""
    with pytest.raises(ValueError, match="requires details\\['reason'\\]"):
        AdapterHealth(ok=False, details={})


def test_adapter_health_rejects_empty_reason():
    """AC-ERR: AdapterHealth(ok=False, details={"reason": ""}) raises ValueError."""
    with pytest.raises(ValueError, match="requires details\\['reason'\\]"):
        AdapterHealth(ok=False, details={"reason": ""})


def test_adapter_health_accepts_reason():
    """AC-ERR: AdapterHealth(ok=False, details={"reason": "binary missing"}) constructs."""
    h = AdapterHealth(ok=False, details={"reason": "binary missing"})
    assert h.ok is False
    assert h.details["reason"] == "binary missing"


def test_adapter_health_ok_unchanged():
    """AC-ERR: AdapterHealth(ok=True, details={}) constructs without error."""
    h = AdapterHealth(ok=True, details={})
    assert h.ok is True
    assert h.details == {}


def test_adapter_health_rejects_whitespace_only_reason():
    """Whitespace-only reason renders as nothing for operators -> reject it."""
    with pytest.raises(ValueError, match="non-whitespace"):
        AdapterHealth(ok=False, details={"reason": "   "})
    with pytest.raises(ValueError, match="non-whitespace"):
        AdapterHealth(ok=False, details={"reason": "\t\n"})
