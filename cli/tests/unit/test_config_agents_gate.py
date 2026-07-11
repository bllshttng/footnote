"""config.agents spawn-gate knobs (x-c5cc): max_live / min_free_gb / worker_qos.

All three coerce invalid values to defaults (fail-open, LD9): the gate is
protective infrastructure and a settings typo must never brick spawning.
"""
from fno.config import AgentsBlock


def test_defaults():
    b = AgentsBlock()
    assert b.max_live == 3
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
