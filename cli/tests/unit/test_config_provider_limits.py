"""x-3f84 W5 plan change 3/4: the `agents.max_lanes` -> `agents.provider_limits`
rename, carrying the ProviderBudget record, with the legacy spelling parsing
forever and ONE deprecation line."""
from __future__ import annotations

import pytest


def test_legacy_spelling_parses_with_deprecation_line(capsys):
    from fno.config import AgentsBlock

    b = AgentsBlock(max_lanes={"zai": 5})
    # The record survives the rename with BOTH dimensions: lanes from the
    # legacy scalar, subagents from zai's built-in budget (x-c703 fill).
    assert b.provider_limits["zai"].lanes == 5
    assert b.provider_limits["zai"].subagents == 1
    err = capsys.readouterr().err
    assert "provider_limits" in err and "max_lanes" in err
    assert not hasattr(b, "max_lanes")


def test_legacy_record_spelling_preserves_subagents(capsys):
    from fno.config import AgentsBlock

    b = AgentsBlock(max_lanes={"zai": {"lanes": 3, "subagents": 2}})
    assert b.provider_limits["zai"].lanes == 3
    assert b.provider_limits["zai"].subagents == 2
    assert "renamed" in capsys.readouterr().err


def test_modern_spelling_wins_when_both_present(capsys):
    from fno.config import AgentsBlock

    b = AgentsBlock(
        provider_limits={"zai": 9},
        max_lanes={"zai": 1},
    )
    assert b.provider_limits["zai"].lanes == 9
    err = capsys.readouterr().err
    assert "both provider_limits and the legacy" in err
    assert "ignoring max_lanes" in err


def test_modern_spelling_is_silent(capsys):
    from fno.config import AgentsBlock

    AgentsBlock(provider_limits={"zai": 5})
    assert capsys.readouterr().err == ""


def test_retired_trigger_parses_warns_once_and_is_ignored(caplog):
    """AC7-HP (x-7783): agents.max_load_per_cpu still parses, prints ONE
    deprecation line naming the decider, and nothing coerces the value - the
    key is ignored, not clamped."""
    import logging

    from fno import config as config_mod
    from fno.config import AgentsBlock

    # The once-guard is process-global: an earlier test in the same run may
    # have consumed this key's single warning. Reset it for determinism.
    config_mod._DEPRECATED_WARNED.discard("agents.max_load_per_cpu")
    with caplog.at_level(logging.WARNING, logger="fno.config"):
        block = AgentsBlock(max_load_per_cpu=10.0)
    assert block.max_load_per_cpu == 10.0
    warnings = [r.message for r in caplog.records if "max_load_per_cpu" in str(r.message)]
    assert len(warnings) == 1
    assert "max_fleet_cpu_share" in warnings[0]
    assert "delete the key" in warnings[0]


def test_retired_backstop_key_is_no_longer_modeled():
    """x-7783 LD2 removed the trigger/backstop pair; x-c588 retired the
    backstop key itself. It no longer parses onto the model: a config that
    still sets it is named as unmodeled on every load and ignored."""
    from fno.config import AgentsBlock

    block = AgentsBlock(max_load_per_cpu=2.0, hard_max_load_per_cpu=1.0)
    assert block.max_load_per_cpu == 2.0
    assert getattr(block, "hard_max_load_per_cpu", None) is None


def test_provider_limits_table_reads_both_spellings():
    # The ONE accessor every reader routes through: new spelling wins, the
    # legacy one still reads (a pre-rename embedded settings object), and a
    # bare object with neither yields an empty table, never a raise.
    from types import SimpleNamespace

    from fno.config import provider_limits_table

    modern = {"zai": {"lanes": 5, "subagents": 1}}
    assert provider_limits_table(SimpleNamespace(provider_limits=modern)) == modern
    legacy = {"zai": 5}
    assert provider_limits_table(SimpleNamespace(max_lanes=legacy)) == legacy
    assert provider_limits_table(SimpleNamespace()) == {}


def test_no_second_agents_leaf_named_max_lanes():
    """AC4-EDGE: after this change, every surviving `max_lanes` leaf belongs to
    `parallel.max_lanes` (the epic-advance cap, LD2) or the legacy alias."""
    from fno.config import AgentsBlock

    agents_fields = {f for f in AgentsBlock.model_fields if f.endswith("max_lanes")}
    assert agents_fields == set(), f"agents.* grew a second max_lanes leaf: {agents_fields}"

# The gate's own provider_limits read moved into the ONE Rust gate
# (spawn_gate_lanes::provider_lanes_cap); its cases live there.
