"""Tests for dispatch-time model/tier resolution (the config-first read side).

The declared inventory is the primary surface: a config-only model resolves
(AC1), per-field fold keeps unnamed fields (AC2), an uninstalled harness
refuses by name (AC3), an empty inventory says so (AC4), unknown capacity
permits (AC10), and the objective key orders candidates. The static tables
are gone; every test declares rows instead.
"""
from __future__ import annotations

import pytest

from fno import route_resolve as rr


def _inv(rows, objective="cheapest-that-clears", prefer="", snapshot=None):
    return rr.inventory_from_rows(
        rows, objective=objective, prefer_harness=prefer, snapshot=snapshot
    )


_FLEET = [
    {"name": "opus-x", "harness": "claude", "model": "claude-opus-5", "band": "high"},
    {"name": "sonnet-x", "harness": "claude", "model": "claude-sonnet-5", "band": "medium"},
    {"name": "sol-x", "harness": "codex", "model": "gpt-5.6-sol", "band": "high"},
    {"name": "luna-x", "harness": "codex", "model": "gpt-5.6-luna", "band": "medium"},
    {"name": "flash-x", "harness": "claude", "model": "glm-5.3-flash", "band": "low"},
]


# --- inventory fold + band derivation -------------------------------------- #


def test_config_only_model_resolves_for_its_band():
    """AC1-HP: a model in no built-in table resolves from the declared row."""
    inv = _inv([{"name": "qwen", "harness": "opencode", "model": "qwen3:30b", "band": "low"}])
    candidate, chain = rr.resolve_grid(
        "low", "p2", {"opencode": "ok"}, inventory=inv
    )
    assert candidate == {"harness": "opencode", "model": "qwen3:30b"}
    assert any("grid candidate opencode/qwen" in step for step in chain)


def test_later_row_overrides_per_field_and_keeps_the_rest():
    """AC2-HP: a same-named later row wins per field; fields it did not name
    keep the earlier row's value."""
    inv = _inv([
        {"name": "qwen", "harness": "opencode", "model": "qwen3:30b", "band": "low"},
        {"name": "qwen", "model": "qwen3:235b"},
    ])
    row = inv.rows["qwen"]
    assert row.model == "qwen3:235b"
    assert row.harness == "opencode"  # unnamed field kept
    assert row.band == "low"  # unnamed field kept


def test_band_from_snapshot_percentile_when_row_leaves_it_unset():
    snap = {"fetched_at": "2026-01-01T00:00:00+00:00", "source": "x", "models": [
        {"name": "mystery", "coding_percentile": 95},
        {"name": "weakling", "coding_percentile": 20},
    ]}
    inv = _inv([
        {"name": "mystery", "harness": "claude", "model": "m-1"},
        {"name": "weakling", "harness": "claude", "model": "w-1"},
    ], snapshot=snap)
    assert inv.rows["mystery"].band == "high"
    assert inv.rows["weakling"].band == ""  # below every floor: unbanded


def test_unbanded_row_is_never_a_grid_candidate():
    inv = _inv([{"name": "mystery", "harness": "claude", "model": "m-1"}])
    candidate, chain = rr.resolve_grid("low", "p2", {"claude": "ok"}, inventory=inv)
    assert candidate is None
    assert chain[-1] == "grid=no-band-candidate"


def test_empty_inventory_records_no_inventory_declared():
    """AC4-EDGE: a virgin install says so; the chain terminal is receiptable."""
    candidate, chain = rr.resolve_grid("high", "p1", {"claude": "ok"}, inventory=rr.Inventory())
    assert candidate is None
    assert chain[-1] == "grid=no-inventory-declared"


def test_absent_difficulty_rounds_up_to_the_strong_band():
    inv = _inv([{"name": "flash-x", "harness": "claude", "model": "f", "band": "low"}])
    candidate, _ = rr.resolve_grid(None, "p2", {"claude": "ok"}, inventory=inv)
    assert candidate is None  # the low row does not clear the high floor


# --- objective ordering ----------------------------------------------------- #


def test_cheapest_that_clears_prefers_declared_cost():
    rows = [
        {"name": "pricey", "harness": "claude", "model": "p", "band": "high",
         "cost_per_mtok_in": 9.9},
        {"name": "frugal", "harness": "claude", "model": "f", "band": "high",
         "cost_per_mtok_in": 1.1},
    ]
    candidate, _ = rr.resolve_grid("high", "p2", {"claude": "ok"}, inventory=_inv(rows))
    assert candidate["model"] == "f"


def test_best_available_prefers_band_then_percentile():
    snap = {"fetched_at": "t", "source": "x", "models": [
        {"name": "sol-x", "coding_percentile": 91},
        {"name": "opus-x", "coding_percentile": 99},
    ]}
    inv = _inv(_FLEET, objective="best-available", snapshot=snap)
    candidate, _ = rr.resolve_grid("medium", "p2", {"claude": "ok", "codex": "ok"}, inventory=inv)
    assert candidate["model"] == "claude-opus-5"  # high band, top percentile


def test_prefer_harness_breaks_ties_without_lowering_the_band():
    inv = _inv(_FLEET, objective="prefer-harness", prefer="claude")
    # medium request: the preferred harness's own >=floor rows come first, so
    # the claude pick wins without ever dipping below the floor.
    candidate, _ = rr.resolve_grid("medium", "p2", {"claude": "ok", "codex": "ok"}, inventory=inv)
    assert candidate["harness"] == "claude"
    # never lowered: a low-only preferred lane does not win a medium request
    low_only = _inv(
        [{"name": "flash-x", "harness": "claude", "model": "glm-5.3-flash", "band": "low"},
         {"name": "luna-x", "harness": "codex", "model": "gpt-5.6-luna", "band": "medium"}],
        objective="prefer-harness", prefer="claude",
    )
    candidate, _ = rr.resolve_grid(
        "medium", "p2", {"claude": "ok", "codex": "ok"}, inventory=low_only
    )
    assert candidate["model"] == "gpt-5.6-luna"
    # high band with claude exhausted: crossing harnesses is allowed without
    # lowering the bar.
    candidate, _ = rr.resolve_grid("high", "p2", {"claude": "exhausted", "codex": "ok"}, inventory=inv)
    assert candidate["model"] == "gpt-5.6-sol"


# --- capacity --------------------------------------------------------------- #


def test_unknown_capacity_permits_and_records_it():
    """AC10-ERR: all-unknown capacity still returns a candidate."""
    candidate, chain = rr.resolve_grid(
        "medium", "p2", {}, inventory=_inv(_FLEET)
    )
    assert candidate is not None
    assert any("capacity=unknown-permitted" in step for step in chain)


def test_only_a_positive_exhausted_marker_removes_a_candidate():
    inv = _inv(_FLEET)
    candidate, _ = rr.resolve_grid(
        "high", "p1", {"claude": "exhausted", "codex": "ok"}, inventory=inv
    )
    assert candidate["harness"] == "codex"
    candidate, chain = rr.resolve_grid(
        "high", "p1", {"claude": "exhausted", "codex": "blocked"}, inventory=inv
    )
    assert candidate is None
    assert chain[-1] == "grid=no-available-candidate"


def test_priority_bends_the_band_p0_high_p3_low():
    inv = _inv(_FLEET)
    c, _ = rr.resolve_grid("low", "p0", {"claude": "ok", "codex": "ok"}, inventory=inv)
    assert c["model"] in ("claude-opus-5", "gpt-5.6-sol")  # p0 -> high band
    c, _ = rr.resolve_grid("high", "p3", {"claude": "ok", "codex": "ok"}, inventory=inv)
    assert c["model"] == "glm-5.3-flash"  # p3 -> low band


def test_grid_does_not_degrade_below_the_requested_band():
    """Round-up: the grid hands an empty tier to the operator's defaults rather
    than quietly giving strong work to a weak row (resolve_tier degrades; the
    grid does not)."""
    inv = _inv([{"name": "flash-x", "harness": "claude", "model": "glm-5.3-flash", "band": "low"}])
    candidate, chain = rr.resolve_grid("high", "p2", {"claude": "ok"}, inventory=inv)
    assert candidate is None
    assert chain[-1] == "grid=no-band-candidate"
    # the task-pin tier resolver still degrades rather than blocking
    model, chain = rr.resolve_tier("high", inventory=inv, provider="claude")
    assert model == "glm-5.3-flash"
    assert any("degrade" in step for step in chain)


# --- effort (third coordinate) ---------------------------------------------- #


def test_effort_varies_with_band_within_the_same_inventory():
    """AC5-HP: high band and low band rows carry different declared effort."""
    rows = [
        {"name": "strong-x", "harness": "codex", "model": "s", "band": "high", "effort": "high"},
        {"name": "cheap-x", "harness": "codex", "model": "c", "band": "low", "effort": "low"},
    ]
    inv = _inv(rows)
    hi, _ = rr.resolve_grid("high", "p2", {"codex": "ok"}, inventory=inv)
    lo, _ = rr.resolve_grid("low", "p2", {"codex": "ok"}, inventory=inv)
    assert hi["effort"] == "high"
    assert lo["effort"] == "low"


def test_no_effort_surface_injects_no_effort_key():
    """AC6-EDGE: agy has no effort surface - the candidate carries no effort."""
    inv = _inv([{"name": "agy-x", "harness": "agy", "model": "a", "band": "high", "effort": "high"}])
    candidate, chain = rr.resolve_grid("high", "p2", {"agy": "ok"}, inventory=inv)
    assert candidate is not None
    assert "effort" not in candidate
    assert any("effort omitted" in step for step in chain)


# --- per-axis constraint + filters ------------------------------------------ #


def test_constrain_harness_picks_within_the_pinned_harness():
    inv = _inv(_FLEET)
    candidate, _ = rr.resolve_grid(
        "high", "p1", {"claude": "ok", "codex": "ok"},
        constrain_harness="codex", inventory=inv,
    )
    assert candidate["harness"] == "codex"
    assert candidate["model"] == "gpt-5.6-sol"


def test_substrate_filter_empties_the_set_with_a_named_reason():
    """AC9-EDGE: a thread substrate no codex row supports records
    constrained-empty rather than cancelling silently."""
    inv = _inv([
        {"name": "sol-x", "harness": "codex", "model": "gpt-5.6-sol", "band": "high"},
    ])
    candidate, chain = rr.resolve_grid(
        "high", "p1", {"codex": "ok"}, substrate="thread", inventory=inv
    )
    assert candidate is None
    assert chain[-1] == "grid=constrained-empty"


def test_uninstalled_harness_refuses_by_name():
    """AC3-ERR: a declared row on a harness fno cannot drive is refused BY
    NAME in the chain, not silently skipped."""
    inv = _inv([{"name": "pi-x", "harness": "pi", "model": "p", "band": "high"}])
    candidate, chain = rr.resolve_grid("high", "p1", {"pi": "ok"}, inventory=inv)
    assert candidate is None
    assert any("refuses pi-x" in step and "not installed" in step for step in chain)


# --- role + protected floors ------------------------------------------------ #


def test_planning_role_floors_the_band_at_high():
    inv = _inv(_FLEET)
    candidate, chain = rr.resolve_grid(
        "low", "p2", {"claude": "ok", "codex": "ok"}, role="planning", inventory=inv
    )
    assert candidate["model"] != "glm-5.3-flash"
    assert any("role(planning)" in step for step in chain)
    candidate, _ = rr.resolve_grid(
        "low", "p2", {"claude": "ok", "codex": "ok"}, role="execution", inventory=inv
    )
    assert candidate["model"] == "glm-5.3-flash"


def test_protected_role_forces_best_available_and_the_floor():
    inv = _inv(_FLEET)
    candidate, chain = rr.resolve_grid(
        "low", "p2", {"claude": "ok", "codex": "ok"},
        protected_role="implement", inventory=inv,
    )
    assert candidate["model"] != "glm-5.3-flash"  # floored to high
    assert any("protected-role(implement)" in step for step in chain)


# --- runtime_capacity: harness -> accounts -> MAX --------------------------- #


def _fake_headroom(monkeypatch, states):
    """Patch runtime_state.headroom with a per-account verdict map."""
    import fno.adapters.providers.runtime_state as rs
    from fno.adapters.providers.runtime_state import Headroom, HeadroomState

    def _h(provider_id, **_kw):
        state = states.get(provider_id, "unknown")
        return Headroom(getattr(HeadroomState, state.upper()), None, source="lock")

    monkeypatch.setattr(rs, "headroom", _h)
    return _h


def test_runtime_capacity_aggregates_max_over_accounts(monkeypatch):
    """AC11-HP: an exhausted record bound to claude reads claude exhausted only
    when EVERY claude account is; one healthy account means usable."""
    inv = _inv([
        {"name": "opus-x", "harness": "claude", "model": "o", "band": "high",
         "account": "primary"},
    ])
    _fake_headroom(monkeypatch, {"primary": "exhausted"})
    cap = rr.runtime_capacity(("claude",), inventory=inv)
    # the harness has one declared account, exhausted -> exhausted
    assert rr._capacity_state(cap["claude"])[0] == "exhausted"
    # a second healthy account (registered record) makes the harness usable
    settings = type("S", (), {"accounts": type("A", (), {"records": [
        {"id": "primary", "harness": "claude"},
        {"id": "backup", "harness": "claude"},
    ]})})()
    _fake_headroom(monkeypatch, {"primary": "exhausted", "backup": "ok"})
    cap = rr.runtime_capacity(("claude",), settings=settings, inventory=inv)
    assert rr._capacity_state(cap["claude"])[0] == "ok"
    assert cap["claude"]["accounts"] == {"primary": "exhausted", "backup": "ok"}
    # exhausted + UNKNOWN is NOT exhausted: exhaustion requires every account
    _fake_headroom(monkeypatch, {"primary": "exhausted"})
    cap = rr.runtime_capacity(("claude",), settings=settings, inventory=inv)
    assert rr._capacity_state(cap["claude"])[0] == "unknown"


def test_harness_accounts_expands_rows_then_registered_records(monkeypatch):
    class _Settings:
        class accounts:
            records = [{"id": "rec-a", "harness": "claude"}, {"id": "rec-b", "harness": "codex"}]
    inv = _inv([
        {"name": "opus-x", "harness": "claude", "model": "o", "route": "zai/glm-5.3"},
        {"name": "flash-x", "harness": "claude", "model": "f", "account": "paid-lane"},
    ])
    # declared rows name their accounts (route vendor, explicit account) and the
    # union adds every registered claude-bound record
    assert rr.harness_accounts("claude", settings=_Settings, inventory=inv) == [
        "zai", "paid-lane", "rec-a",
    ]
    # no declared row for codex -> every registered record bound to codex
    assert rr.harness_accounts("codex", settings=_Settings, inventory=inv) == ["rec-b"]


def test_runtime_capacity_records_window_absent_with_no_accounts(monkeypatch):
    cap = rr.runtime_capacity(
        ("claude",), inventory=rr.Inventory()
    )
    assert cap["claude"]["window"] == "absent"
    assert cap["claude"]["state"] == "unknown"


# --- resolve_tier / node_model (inventory-backed) --------------------------- #


def test_tier_resolves_declared_row_and_scopes_to_harness():
    inv = _inv(_FLEET)
    # cheapest-that-clears with no cost/percentile: weakest band that clears
    assert rr.resolve_tier("medium", inventory=inv, provider="claude")[0] == "claude-sonnet-5"
    assert rr.resolve_tier("medium", inventory=inv, provider="banana")[0] is None
    assert rr.resolve_tier("banana", inventory=inv)[0] is None


def test_tier_empty_inventory_is_provider_default():
    model, chain = rr.resolve_tier("low", inventory=rr.Inventory())
    assert model is None
    assert any("no declared inventory" in step for step in chain)


def test_node_model_reads_pin_and_band():
    inv = _inv(_FLEET)
    assert rr.node_model({"model": "glm-5.2"}, inventory=inv) == "glm-5.2"
    assert rr.node_model(
        {"difficulty": "low"}, inventory=inv, provider="claude"
    ) == "glm-5.3-flash"
    assert rr.node_model(
        {"difficulty": "high"}, inventory=inv, provider="claude", resolve_difficulty=False
    ) is None


def test_node_model_none_provider_defaults_to_claude(monkeypatch):
    monkeypatch.setenv("CODEX_SANDBOX", "1")
    inv = _inv(_FLEET)
    assert rr.node_model({"difficulty": "medium"}, inventory=inv) == "claude-sonnet-5"


def test_node_model_degrades_on_resolver_error(monkeypatch):
    def _boom(**_kw):
        raise RuntimeError("resolver boom")

    monkeypatch.setattr(rr, "resolve_dispatch_model", _boom)
    assert rr.node_model({"model": "glm-5.2"}, inventory=rr.Inventory()) == "glm-5.2"


def test_retired_tier_params_are_gone():
    import inspect

    params = inspect.signature(rr.resolve_dispatch_model).parameters
    assert "task_tier" not in params and "plan_tier" not in params
    with pytest.raises(TypeError):
        rr.resolve_dispatch_model(task_tier="low")


def test_precedence_chain_labels_sources():
    inv = _inv(_FLEET)
    model, source, _ = rr.resolve_dispatch_model(
        explicit="pinned-x", task_difficulty="high", inventory=inv
    )
    assert (model, source) == ("pinned-x", "explicit")
    model, source, _ = rr.resolve_dispatch_model(
        task_model="task-x", task_difficulty="high", inventory=inv
    )
    assert (model, source) == ("task-x", "task-pin")
    model, source, _ = rr.resolve_dispatch_model(
        task_difficulty="low", inventory=inv, provider="claude"
    )
    assert model == "glm-5.3-flash" and source == "task-difficulty(low)"
    model, source, _ = rr.resolve_dispatch_model(inventory=inv)
    assert model is None and source == "provider-default"
