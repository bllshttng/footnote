"""Tests for config.target.handoff.king_used_pct_trigger (x-7685, US5/AC8).

A king hands off earlier than a teammate (40 vs 50), so the cross-field
validator refuses king_used_pct_trigger >= used_pct_trigger. The refusal
MESSAGE is the deliverable: it names both values and carries the rationale, so
the person trying to "tidy" 40 up to 50 reads why before they can. AC8.
"""
from __future__ import annotations

import pytest
from pydantic import ValidationError

from fno.config import HandoffBlock

_RATIONALE = "propagates into every ruling"


def test_default_king_trigger_is_40():
    assert HandoffBlock().king_used_pct_trigger == 40


def test_king_trigger_39_with_teammate_50_accepted():
    block = HandoffBlock(used_pct_trigger=50, king_used_pct_trigger=39)
    assert block.king_used_pct_trigger == 39


def test_king_trigger_equal_to_teammate_is_refused_with_both_values():
    with pytest.raises(ValidationError) as exc:
        HandoffBlock(used_pct_trigger=50, king_used_pct_trigger=50)
    msg = str(exc.value)
    # Names both values so the conflict is visible, and carries the rationale
    # sentence that defends the 40.
    assert "king_used_pct_trigger must be BELOW used_pct_trigger" in msg
    assert "got 50" in msg
    assert "teammate trigger 50" in msg
    assert _RATIONALE in msg


def test_king_trigger_above_teammate_is_refused():
    with pytest.raises(ValidationError):
        HandoffBlock(used_pct_trigger=50, king_used_pct_trigger=60)


def test_king_trigger_range_1_to_100_enforced():
    with pytest.raises(ValidationError) as exc:
        HandoffBlock(king_used_pct_trigger=0)
    assert "1-100" in str(exc.value)
    with pytest.raises(ValidationError):
        HandoffBlock(king_used_pct_trigger=101)


def test_king_just_below_teammate_accepted():
    # The boundary is strict-<, so king = teammate - 1 is the tightest accept.
    block = HandoffBlock(used_pct_trigger=50, king_used_pct_trigger=49)
    assert block.king_used_pct_trigger == 49
