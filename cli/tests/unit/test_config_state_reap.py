"""State-file reap retention defaults and visible fail-safe degradation."""

from __future__ import annotations

import pytest

from fno.config import AgentsBlock
from fno.config._sweeps import DEGRADED

DEFAULTS = {
    "enabled": True,
    "locks_retain_days": 7,
    "expired_claims_retain_days": 30,
    "pr_status_cache_retain_days": 14,
}


def test_state_reap_defaults_match_the_rust_daemon() -> None:
    assert AgentsBlock().state_reap.model_dump() == DEFAULTS


def test_state_reap_configured_values_are_honored() -> None:
    configured = {
        "enabled": False,
        "locks_retain_days": 2,
        "expired_claims_retain_days": 45,
        "pr_status_cache_retain_days": 21,
    }
    block = AgentsBlock(state_reap=configured).state_reap
    assert block.model_dump() == configured


@pytest.mark.parametrize(
    "configured",
    [
        {
            "locks_retain_days": "weekly",
            "expired_claims_retain_days": True,
            "pr_status_cache_retain_days": None,
        },
        {
            "locks_retain_days": 0,
            "expired_claims_retain_days": -1,
            "pr_status_cache_retain_days": -14,
        },
    ],
    ids=("invalid", "nonpositive"),
)
def test_bad_state_reap_days_degrade_and_name_exact_keys(
    configured: dict[str, object],
) -> None:
    DEGRADED.clear()
    block = AgentsBlock(state_reap=configured).state_reap
    assert block.model_dump() == DEFAULTS
    assert set(DEGRADED) == {f"agents.state_reap.{key}" for key in configured}
