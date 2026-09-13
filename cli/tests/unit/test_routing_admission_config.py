"""config.routing.admission: the opt-in reservation policy block (x-1afa).

The block never raises at load - one typo in an opt-in block must not take
down every command - so raw values travel and the admission owner refuses an
armed-but-tainted table (the exact-error tests run against the verb in
test_routing_admission.py, where the validation lives).
"""

from __future__ import annotations

from fno.config import SettingsModel
from fno.config.routing_blocks import RoutingAdmissionBlock, resolve_admission_policy


def _settings(payload: dict) -> SettingsModel:
    return SettingsModel.model_validate({"routing": {"admission": payload}})


def test_disabled_block_gates_nothing_and_defaults():
    s = _settings({})
    block = s.routing.admission
    assert block.enabled is False
    assert block.max_inflight_per_pool == 3
    assert block.reservation_ttl_seconds == 900
    assert block.demand_pct == {}
    assert resolve_admission_policy(s) is None


def test_armed_policy_resolves_to_the_block_with_tables_intact():
    s = _settings(
        {
            "enabled": True,
            "demand_pct": {"review": {"default": 15, "high": 25}},
            "reserve_pct": {"default": {"default": 10}},
        }
    )
    p = resolve_admission_policy(s)
    assert p is not None
    # The tables travel intact; the difficulty lookup and the validation both
    # live in the Rust owner, which prices every receipt with the row it chose.
    assert p.demand_pct["review"]["default"] == 15
    assert p.demand_pct["review"]["high"] == 25
    assert p.reserve_pct["default"]["default"] == 10


def test_armed_raw_values_travel_untouched():
    """No load-time stripping: the owner sees exactly what the operator wrote
    and refuses the bad leaf at the seam."""
    s = _settings(
        {
            "enabled": True,
            "reserve_pct": {"review": {"default": 150}},
            "demand_pct": {"do": {"high": "$5"}},
        }
    )
    block = resolve_admission_policy(s)
    assert block is not None
    assert block.reserve_pct["review"]["default"] == 150
    assert block.demand_pct["do"]["high"] == "$5"
    assert not block.model_extra


def test_unknown_block_fields_are_kept_for_the_owner():
    s = _settings({"enabled": True, "no_such_knob": True})
    block = resolve_admission_policy(s)
    assert block is not None
    assert block.model_extra == {"no_such_knob": True}


def test_disabled_malformed_table_degrades_silently():
    s = _settings({"enabled": False, "demand_pct": {"do": {"high": 999}}})
    assert s.routing.admission.enabled is False
    assert resolve_admission_policy(s) is None


def test_unreadable_settings_gate_nothing():
    class _Broken:
        @property
        def routing(self):
            raise RuntimeError("unreadable")

    assert resolve_admission_policy(_Broken()) is None
    resolve_admission_policy()  # live settings: never raises


def test_block_is_a_plain_pydantic_model():
    block = RoutingAdmissionBlock()
    assert block.enabled is False
    assert not block.model_extra
