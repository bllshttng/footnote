"""config.routing.admission: the opt-in reservation policy block (x-1afa).

The block never raises at load - one typo in an opt-in block must not take
down every command - so an armed-but-malformed table degrades to sane defaults
with the exact field-and-unit error recorded, and the admission owner refuses
on it. AC1-HP and AC1-ERR pin both halves here.
"""
from __future__ import annotations

from fno.config import SettingsModel
from fno.config._routing_admission import (
    DEGRADED,
    RoutingAdmissionBlock,
    admission_config_errors,
    resolve_admission_policy,
)


def _settings(payload: dict) -> SettingsModel:
    return SettingsModel.model_validate({"routing": {"admission": payload}})


def setup_function() -> None:
    DEGRADED.clear()


def test_disabled_block_gates_nothing_and_defaults():
    s = _settings({})
    block = s.routing.admission
    assert block.enabled is False
    assert block.max_inflight_per_pool == 3
    assert block.reservation_ttl_seconds == 900
    assert block.demand_pct == {}
    assert resolve_admission_policy(s).enabled is False


def test_armed_policy_resolves_demand_and_reserve_by_difficulty():
    s = _settings({
        "enabled": True,
        "demand_pct": {"review": {"default": 15, "high": 25}},
        "reserve_pct": {"default": {"default": 10}},
    })
    p = resolve_admission_policy(s)
    assert p.enabled is True
    # The most specific row at or below the band wins.
    assert p.demand_for("review", "medium") == 15
    assert p.demand_for("review", "high") == 25
    # An undeclared verb falls back to the default verb row.
    assert p.demand_for("do", "high") == 0.0
    assert p.reserve_for("do", "low") == 10


def test_armed_percentage_out_of_range_refuses_with_exact_field_and_unit():
    _settings({
        "enabled": True,
        "reserve_pct": {"review": {"default": 150}},
    })
    errors = admission_config_errors()
    assert any(
        "routing.admission.reserve_pct.review.default" in message
        and "[0, 100]" in message
        for message in errors.values()
    ), errors
    # The seam sees the same taint: an armed policy refuses, it never guesses.
    s = _settings({
        "enabled": True,
        "reserve_pct": {"review": {"default": 150}},
    })
    p = resolve_admission_policy(s)
    assert p.enabled is True
    assert p.config_errors


def test_armed_currency_amount_gets_the_unit_error_not_the_range_error():
    _settings({
        "enabled": True,
        "demand_pct": {"do": {"high": "$5"}},
    })
    message = next(iter(admission_config_errors().values()))
    assert "currency amount" in message
    assert "routing.admission.demand_pct.do.high" in message
    assert "unsupported" in message


def test_armed_unknown_difficulty_and_field_are_refused():
    _settings({
        "enabled": True,
        "demand_pct": {"do": {"impossible": 10}},
        "no_such_knob": True,
    })
    messages = " | ".join(admission_config_errors().values())
    assert "unknown difficulty 'impossible'" in messages
    assert "unknown field(s) no_such_knob" in messages


def test_disabled_malformed_table_degrades_silently():
    s = _settings({"enabled": False, "demand_pct": {"do": {"high": 999}}})
    assert s.routing.admission.enabled is False
    assert admission_config_errors() == {}
    assert resolve_admission_policy(s).enabled is False


def test_armed_bad_bounds_degrade_to_defaults_with_errors():
    s = _settings({
        "enabled": True,
        "max_inflight_per_pool": 0,
        "reservation_ttl_seconds": -5,
    })
    block = s.routing.admission
    assert block.max_inflight_per_pool == 3
    assert block.reservation_ttl_seconds == 900
    assert len(admission_config_errors()) == 2
    assert resolve_admission_policy(s).config_errors


def test_two_api_records_one_pool_and_distinct_undeclared_records():
    from fno.adapters.providers.model import ProviderRecord

    def _record(record_id: str, pool: str | None) -> ProviderRecord:
        return ProviderRecord(
            id=record_id,
            name=record_id,
            harness="claude",
            auth="api_key",
            env={"ANTHROPIC_API_KEY": "sk-test"},
            quota_pool=pool,
        )

    assert _record("rec-a", "family").quota_pool == "family"
    assert _record("rec-b", "family").quota_pool == "family"
    assert _record("rec-c", None).quota_pool is None
