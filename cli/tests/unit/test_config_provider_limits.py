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


def test_gate_reads_provider_limits_not_the_legacy_leaf(monkeypatch, capsys):
    """AC3-HP: the spawn gate applies the provider_limits cap exactly as
    max_lanes was applied before the rename. Proven on the provider-cap
    terminal: a zai-routed spawn reaches the cap read (and its refusal) only
    through provider_limits."""
    from fno.agents import spawn_gate
    from fno.config import ProviderBudget

    def fake_run_gate_settings():
        class _A:
            max_live = 3
            min_free_gb = 0.0
            max_load_per_cpu = 0.0
            provider_limits = {"zai": ProviderBudget(lanes=5, subagents=1)}

            def __getattr__(self, name):  # any legacy reader must miss loudly
                raise AttributeError(name)

        class _S:
            agents = _A()

        return _S()

    monkeypatch.setattr("fno.config.load_settings", fake_run_gate_settings)
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    # A census that cannot answer forces the provider-cap arm to refuse
    # immediately - the arm that read provider_limits on the way in.
    def broken_census():
        raise spawn_gate.ProviderCountUnavailable("count unavailable")

    monkeypatch.setattr(spawn_gate, "census", broken_census)
    with pytest.raises(SystemExit) as exc:
        spawn_gate.run_gate("w", "bg", route_provider="zai")
    assert exc.value.code == spawn_gate.EXIT_PROVIDER_CAP
    assert "provider zai, cap 5" in capsys.readouterr().err


def test_no_second_agents_leaf_named_max_lanes():
    """AC4-EDGE: after this change, every surviving `max_lanes` leaf belongs to
    `parallel.max_lanes` (the epic-advance cap, LD2) or the legacy alias."""
    from fno.config import AgentsBlock

    agents_fields = {f for f in AgentsBlock.model_fields if f.endswith("max_lanes")}
    assert agents_fields == set(), f"agents.* grew a second max_lanes leaf: {agents_fields}"
