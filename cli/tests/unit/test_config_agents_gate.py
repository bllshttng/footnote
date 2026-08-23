"""config.agents spawn-gate knobs (x-c5cc): max_live / min_free_gb / worker_qos.

All three coerce invalid values to defaults (fail-open, LD9): the gate is
protective infrastructure and a settings typo must never brick spawning.
"""
from fno.config import AgentsBlock


def test_provider_loader_reserved_keys_match_agents_schema():
    from fno.adapters.providers.loader import _AGENTS_RESERVED_KEYS

    # `max_lanes` is the one tolerated legacy spelling (renamed provider_limits,
    # x-3f84 W5): still parsed by the block's before-validator, so it stays
    # reserved against provider pins.
    assert _AGENTS_RESERVED_KEYS == set(AgentsBlock.model_fields) | {"max_lanes"}


def test_defaults():
    b = AgentsBlock()
    assert b.max_live == 3
    assert b.provider_limits["zai"].lanes == 5
    assert b.provider_limits["zai"].subagents == 1
    assert b.min_free_gb == 4.0
    assert b.worker_qos == "utility"


def test_valid_values_pass_through():
    b = AgentsBlock(max_live=7, min_free_gb=2.5, worker_qos="off")
    assert b.max_live == 7
    assert b.min_free_gb == 2.5
    assert b.worker_qos == "off"


def test_max_live_below_one_coerces_to_default():
    assert AgentsBlock(max_live=0).max_live == 3
    assert AgentsBlock(max_live=-2).max_live == 3
    assert AgentsBlock(max_live="banana").max_live == 3
    assert AgentsBlock(max_live=True).max_live == 3


def test_provider_limits_is_per_provider_and_invalid_shape_keeps_safe_default():
    # x-c703 widened the value from a bare lane count to a ProviderBudget; x-3f84
    # W5 renamed the key. The scalar spelling stays legal and reads as `lanes`;
    # zai's built-in `subagents = 1` rides along, because an install that
    # predates the dimension is exactly the one that must not fan out on a
    # shared account.
    lanes = {
        name: (b.lanes, b.subagents)
        for name, b in AgentsBlock(
            provider_limits={"zai": 2, "openai": 7}
        ).provider_limits.items()
    }
    assert lanes == {"zai": (2, 1), "openai": (7, None)}
    assert AgentsBlock(provider_limits={}).provider_limits == {}
    for broken in ({"zai": 0}, "broken", {"zai": True}, {"zai": {"bogus": 1}}):
        restored = AgentsBlock(provider_limits=broken).provider_limits["zai"]
        assert (restored.lanes, restored.subagents) == (5, 1), broken


def test_min_free_gb_zero_is_valid_disable():
    # <= 0 means "guard disabled", NOT an invalid value.
    assert AgentsBlock(min_free_gb=0).min_free_gb == 0.0
    assert AgentsBlock(min_free_gb=-1).min_free_gb == -1.0


def test_min_free_gb_unparseable_coerces_to_default():
    assert AgentsBlock(min_free_gb="banana").min_free_gb == 4.0
    assert AgentsBlock(min_free_gb=None).min_free_gb == 4.0
    assert AgentsBlock(min_free_gb=True).min_free_gb == 4.0


def test_worker_qos_unknown_coerces_to_utility():
    assert AgentsBlock(worker_qos="turbo").worker_qos == "utility"
    assert AgentsBlock(worker_qos=None).worker_qos == "utility"
    assert AgentsBlock(worker_qos="OFF").worker_qos == "off"


def test_spawn_defaults_unset_by_default():
    # US7: empty string = unset (the spawn_permission_mode convention).
    d = AgentsBlock().defaults
    assert d.provider == ""
    assert d.model == ""
    assert d.effort == ""


def test_spawn_defaults_values_pass_through():
    b = AgentsBlock(defaults={"provider": "codex", "model": "gpt-5.6-sol", "effort": "high"})
    assert b.defaults.provider == "codex"
    assert b.defaults.model == "gpt-5.6-sol"
    assert b.defaults.effort == "high"


def test_spawn_defaults_non_mapping_degrades_to_unset():
    # A scalar/list/null `agents.defaults:` must not raise out of the load.
    for bad in ("banana", ["x"], None, 3):
        assert AgentsBlock(defaults=bad).defaults.provider == ""


def test_dead_row_grace_bare_integer_is_the_default_shape():
    assert AgentsBlock().dead_row_grace == 3600
    assert AgentsBlock(dead_row_grace=7200).dead_row_grace == 7200


def test_dead_row_grace_accepts_a_per_harness_table():
    # x-9de7 task 6: agents.dead_row_grace.<harness> = <seconds>, mirroring
    # the Rust resolver (agents_config::dead_row_grace_secs). A harness
    # absent from the table is not this model's job to fill in -- that
    # fallback lives in the Rust resolver, which this type just has to admit.
    b = AgentsBlock(dead_row_grace={"codex": 28800})
    assert b.dead_row_grace == {"codex": 28800}


def test_dead_row_grace_rejects_a_negative_value_in_either_shape():
    import pytest
    from pydantic import ValidationError

    with pytest.raises(ValidationError):
        AgentsBlock(dead_row_grace=-1)
    with pytest.raises(ValidationError):
        AgentsBlock(dead_row_grace={"codex": -1})
