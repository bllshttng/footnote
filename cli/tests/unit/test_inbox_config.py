"""config.inbox.unclaimed_ttl knob (x-39a4 task 1.1).

The sender-unclaimed TTL gate reads this knob, so it must exist as a real
modeled leaf with the locked 1800s (30m) default and a non-negative guard.
"""
from __future__ import annotations

import pytest

from fno.config import SettingsModel


def test_unclaimed_ttl_default_is_1800() -> None:
    assert SettingsModel().inbox.unclaimed_ttl == 1800


def test_unclaimed_ttl_round_trips() -> None:
    m = SettingsModel.model_validate({"inbox": {"unclaimed_ttl": 600}})
    assert m.inbox.unclaimed_ttl == 600


def test_unclaimed_ttl_rejects_negative() -> None:
    with pytest.raises(ValueError):
        SettingsModel.model_validate({"inbox": {"unclaimed_ttl": -1}})
