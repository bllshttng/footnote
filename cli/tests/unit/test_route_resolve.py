"""Tests for dispatch-time model/tier resolution (the config-first read side).

The declared inventory is the primary surface: a config-only model resolves
(AC1), per-field fold keeps unnamed fields (AC2), an uninstalled harness
refuses by name (AC3), an empty inventory says so (AC4), unknown capacity
permits (AC10), and the objective key orders candidates. The static tables
are gone; every test declares rows instead.
"""
from __future__ import annotations

import pytest

from fno.rust_binary import find_dev_binary

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)"
,
)

from fno import route_resolve as rr



def _grid(difficulty, priority, capacity, inv=None, **kw):
    """The grid leg rides the verb: resolve_slot with no lanes grids."""
    node = {"priority": priority}
    if difficulty is not None:
        node["difficulty"] = difficulty
    return rr.resolve_slot("target", node, capacity, inventory=inv, **kw)[:2]

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
    candidate, chain = _grid("low", "p2", {"opencode": "ok"}, inv=inv)
    assert candidate == {"harness": "opencode", "model": "qwen3:30b"}
    assert any("grid candidate opencode/qwen" in step for step in chain)


def test_declared_rows_read_the_real_loader_models(tmp_path, monkeypatch):
    """x-947c: routing.models arrives from the loader as pydantic rows, not
    mappings; _declared_rows must read them, never drop them at a Mapping
    filter. Drives the REAL loader over a config file - no hand-built dict."""
    from fno.config import load_settings

    cfg = tmp_path / "config.toml"
    cfg.write_text(
        "[routing]\n"
        "[[routing.models]]\n"
        'name = "zai-flash"\n'
        'harness = "claude"\n'
        'model = "glm-5.3-flash[1m]"\n'
        'route = "zai/glm-5.3-flash[1m]"\n'
        'account = "zai"\n'
        'band = "medium"\n',
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    rows = rr._declared_rows(load_settings())
    assert rows["zai-flash"] == {
        "name": "zai-flash",
        "harness": "claude",
        "model": "glm-5.3-flash[1m]",
        "route": "zai/glm-5.3-flash[1m]",
        "account": "zai",
        "band": "medium",
        "effort": "",
        "operator_view": "",
    }


def test_grid_candidate_carries_route_and_account():
    """AC2-HP (x-b545): the grid leg's candidate carries the declared row's
    route and account, the same facts the lane leg always emitted. A routeless
    row emits neither key, so every anthropic row is unchanged."""
    inv = _inv([
        {"name": "flash", "harness": "claude", "model": "glm-5.3-flash[1m]",
         "band": "high", "route": "zai/glm-5.3-flash[1m]", "account": "zai-main"},
    ])
    candidate, _chain = _grid(
        "high", "p2", {"claude": {"state": "ok", "accounts": {"zai-main": "ok"}}}, inv=inv
    )
    assert candidate["route"] == "zai/glm-5.3-flash[1m]"
    assert candidate["account"] == "zai-main"
    inv = _inv([{"name": "bare", "harness": "claude", "model": "m-1", "band": "high"}])
    candidate, _chain = _grid("high", "p2", {"claude": "ok"}, inv=inv)
    assert "route" not in candidate
    assert "account" not in candidate


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


def test_unbanded_row_is_a_candidate_at_every_band():
    """AC1-HP: one unbanded row is the whole inventory and still answers: band
    is optional, so a single-model fleet is routable with one declared row."""
    inv = _inv([{"name": "mystery", "harness": "claude", "model": "m-1"}])
    for difficulty in ("low", "high", None):
        candidate, chain = _grid(difficulty, "p2", {"claude": "ok"}, inv=inv)
        assert candidate == {"harness": "claude", "model": "m-1"}, difficulty
        assert any("band=unbanded" in step for step in chain), difficulty


def test_banded_row_that_clears_outranks_an_unbanded_one():
    """AC1-EDGE: unbanded rows rank AFTER the banded rows that clear, in
    declared order - they are admitted everywhere, never preferred."""
    inv = _inv([
        {"name": "banded", "harness": "claude", "model": "b-1", "band": "low"},
        {"name": "unbanded", "harness": "claude", "model": "u-1"},
    ])
    candidate, chain = _grid("low", "p2", {"claude": "ok"}, inv=inv)
    assert candidate["model"] == "b-1"
    assert not any("band=unbanded" in step for step in chain)
    # the unbanded row is still the next candidate once the banded one is out
    # of range: a high floor the low band cannot clear leaves the unbanded row
    candidate, chain = _grid("high", "p2", {"claude": "ok"}, inv=inv)
    assert candidate["model"] == "u-1"
    assert any("band=unbanded" in step for step in chain)


def test_row_naming_no_account_reads_the_harness_aggregate():
    """AC2-EDGE: an account-less row reads the MAX aggregate exactly as before
    per-account resolution existed."""
    inv = _inv([{"name": "any-lane", "harness": "claude", "model": "a-1", "band": "low"}])
    candidate, chain = _grid("low", "p2", {"claude": {"state": "ok", "accounts": {"a": "exhausted", "b": "exhausted"}}}, inv=inv, )
    # the accounts are dead but the aggregate the row reads is ok
    assert candidate is not None
    assert any("capacity=ok" in step for step in chain)


def test_lane_on_an_exhausted_account_is_skipped_when_a_sibling_account_is_healthy():
    """AC2-HP: quota locks out at the ACCOUNT, so the row pinned to the dead
    account skips and the sibling on the healthy account wins."""
    inv = _inv([
        {"name": "row-a", "harness": "claude", "model": "a-1", "band": "low",
         "account": "a"},
        {"name": "row-b", "harness": "claude", "model": "b-1", "band": "low",
         "account": "b"},
    ])
    candidate, chain = _grid("low", "p2", {"claude": {"state": "ok", "accounts": {"a": "exhausted", "b": "ok"}}}, inv=inv, )
    assert candidate["model"] == "b-1"
    assert any(
        "grid skip claude/row-a capacity=exhausted" in step for step in chain
    )


def test_row_account_absent_from_the_snapshot_reads_unknown_and_permits():
    """An account the capacity snapshot does not name reads unknown, which
    permits - the same posture the harness-wide unknown already has."""
    inv = _inv([{"name": "pinned", "harness": "claude", "model": "p-1", "band": "low",
                 "account": "unlisted"}])
    candidate, chain = _grid("low", "p2", {"claude": {"state": "ok", "accounts": {"a": "exhausted"}}}, inv=inv, )
    assert candidate is not None
    assert any("capacity=unknown-permitted" in step for step in chain)


def test_empty_inventory_records_no_inventory_declared():
    """AC4-EDGE: a virgin install says so; the chain terminal is receiptable."""
    candidate, chain = _grid("high", "p1", {"claude": "ok"}, inv=rr.Inventory())
    assert candidate is None
    assert chain[-1] == "grid=no-inventory-declared"


def test_absent_difficulty_rounds_up_to_the_strong_band():
    inv = _inv([{"name": "flash-x", "harness": "claude", "model": "f", "band": "low"}])
    candidate, _ = _grid(None, "p2", {"claude": "ok"}, inv=inv)
    assert candidate is None  # the low row does not clear the high floor


# --- objective ordering ----------------------------------------------------- #


def test_cheapest_that_clears_prefers_declared_cost():
    rows = [
        {"name": "pricey", "harness": "claude", "model": "p", "band": "high",
         "cost_per_mtok_in": 9.9},
        {"name": "frugal", "harness": "claude", "model": "f", "band": "high",
         "cost_per_mtok_in": 1.1},
    ]
    candidate, _ = _grid("high", "p2", {"claude": "ok"}, inv=_inv(rows))
    assert candidate["model"] == "f"


def test_best_available_prefers_band_then_percentile():
    snap = {"fetched_at": "t", "source": "x", "models": [
        {"name": "sol-x", "coding_percentile": 91},
        {"name": "opus-x", "coding_percentile": 99},
    ]}
    inv = _inv(_FLEET, objective="best-available", snapshot=snap)
    candidate, _ = _grid("medium", "p2", {"claude": "ok", "codex": "ok"}, inv=inv)
    assert candidate["model"] == "claude-opus-5"  # high band, top percentile


def test_prefer_harness_breaks_ties_without_lowering_the_band():
    inv = _inv(_FLEET, objective="prefer-harness", prefer="claude")
    # medium request: the preferred harness's own >=floor rows come first, so
    # the claude pick wins without ever dipping below the floor.
    candidate, _ = _grid("medium", "p2", {"claude": "ok", "codex": "ok"}, inv=inv)
    assert candidate["harness"] == "claude"
    # never lowered: a low-only preferred lane does not win a medium request
    low_only = _inv(
        [{"name": "flash-x", "harness": "claude", "model": "glm-5.3-flash", "band": "low"},
         {"name": "luna-x", "harness": "codex", "model": "gpt-5.6-luna", "band": "medium"}],
        objective="prefer-harness", prefer="claude",
    )
    candidate, _ = _grid("medium", "p2", {"claude": "ok", "codex": "ok"}, inv=low_only)
    assert candidate["model"] == "gpt-5.6-luna"
    # high band with claude exhausted: crossing harnesses is allowed without
    # lowering the bar.
    candidate, _ = _grid("high", "p2", {"claude": "exhausted", "codex": "ok"}, inv=inv)
    assert candidate["model"] == "gpt-5.6-sol"


# --- capacity --------------------------------------------------------------- #


def test_unknown_capacity_permits_and_records_it():
    """AC10-ERR: all-unknown capacity still returns a candidate."""
    candidate, chain = _grid("medium", "p2", {}, inv=_inv(_FLEET)
    )
    assert candidate is not None
    assert any("capacity=unknown-permitted" in step for step in chain)


def test_only_a_positive_exhausted_marker_removes_a_candidate():
    inv = _inv(_FLEET)
    candidate, _ = _grid("high", "p1", {"claude": "exhausted", "codex": "ok"}, inv=inv)
    assert candidate["harness"] == "codex"
    candidate, chain = _grid("high", "p1", {"claude": "exhausted", "codex": "blocked"}, inv=inv)
    assert candidate is None
    assert chain[-1] == "grid=no-available-candidate"


def test_priority_bends_the_band_p0_high_p3_low():
    inv = _inv(_FLEET)
    c, _ = _grid("low", "p0", {"claude": "ok", "codex": "ok"}, inv=inv)
    assert c["model"] in ("claude-opus-5", "gpt-5.6-sol")  # p0 -> high band
    c, _ = _grid("high", "p3", {"claude": "ok", "codex": "ok"}, inv=inv)
    assert c["model"] == "glm-5.3-flash"  # p3 -> low band


def test_grid_does_not_degrade_below_the_requested_band():
    """Round-up: the grid hands an empty tier to the operator's defaults rather
    than quietly giving strong work to a weak row (resolve_tier degrades; the
    grid does not)."""
    inv = _inv([{"name": "flash-x", "harness": "claude", "model": "glm-5.3-flash", "band": "low"}])
    candidate, chain = _grid("high", "p2", {"claude": "ok"}, inv=inv)
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
    hi, _ = _grid("high", "p2", {"codex": "ok"}, inv=inv)
    lo, _ = _grid("low", "p2", {"codex": "ok"}, inv=inv)
    assert hi["effort"] == "high"
    assert lo["effort"] == "low"


def test_no_effort_surface_injects_no_effort_key():
    """AC6-EDGE: gemini has no effort surface - the candidate carries no effort.

    This was agy's test until agy was measured into the support set: its own
    ``--help`` declares ``--effort (low|medium|high)`` on 1.1.24, so the grid
    now injects effort for agy like it does for claude. gemini is the binary
    the deny set actually names."""
    inv = _inv([{"name": "gem-x", "harness": "gemini", "model": "g", "band": "high", "effort": "high"}])
    candidate, chain = _grid("high", "p2", {"gemini": "ok"}, inv=inv)
    assert candidate is not None
    assert "effort" not in candidate
    assert any("effort omitted" in step for step in chain)


# --- per-axis constraint + filters ------------------------------------------ #


def test_constrain_harness_picks_within_the_pinned_harness():
    inv = _inv(_FLEET)
    candidate, _ = _grid("high", "p1", {"claude": "ok", "codex": "ok"}, inv=inv, constrain_harness="codex", )
    assert candidate["harness"] == "codex"
    assert candidate["model"] == "gpt-5.6-sol"


def test_substrate_filter_empties_the_set_with_a_named_reason():
    """AC9-EDGE: a thread substrate no declared row supports records
    constrained-empty rather than cancelling silently.

    Reads gemini, whose spawn claim is absent and whose command surface is
    refused for good. This test named codex until codex gained a verified
    thread lane, then opencode until its spawn claim measured native; the
    edge case is about a harness without a seat, so it follows the
    capability rather than the harness name.
    """
    inv = _inv([
        {"name": "gem-x", "harness": "gemini", "model": "gm-big", "band": "high"},
    ])
    candidate, chain = _grid("high", "p1", {"gemini": "ok"}, inv=inv, substrate="thread")
    assert candidate is None
    assert chain[-1] == "grid=constrained-empty"


def test_uninstalled_harness_refuses_by_name():
    """AC3-ERR: a declared row on a harness fno cannot drive is refused BY
    NAME in the chain, not silently skipped.

    The harness name here must stay one fno will never know. It used to be
    "pi", which stopped working the day pi was onboarded and made this test
    assert the opposite of what it means.
    """
    inv = _inv(
        [{"name": "ghost-x", "harness": "ghostharness", "model": "g", "band": "high"}]
    )
    candidate, chain = _grid("high", "p1", {"ghostharness": "ok"}, inv=inv)
    assert candidate is None
    assert any("refuses ghost-x" in step and "not installed" in step for step in chain)


# --- role + protected floors ------------------------------------------------ #


def test_planning_role_floors_the_band_at_high():
    inv = _inv(_FLEET)
    candidate, chain = _grid("low", "p2", {"claude": "ok", "codex": "ok"}, inv=inv, role="planning")
    assert candidate["model"] != "glm-5.3-flash"
    assert any("role(planning)" in step for step in chain)
    candidate, _ = _grid("low", "p2", {"claude": "ok", "codex": "ok"}, inv=inv, role="execution")
    assert candidate["model"] == "glm-5.3-flash"


def test_protected_role_forces_best_available_and_the_floor():
    inv = _inv(_FLEET)
    candidate, chain = _grid("low", "p2", {"claude": "ok", "codex": "ok"}, inv=inv, protected_role="implement", )
    assert candidate["model"] != "glm-5.3-flash"  # floored to high
    assert any("protected-role(implement)" in step for step in chain)


# --- runtime_capacity: harness -> accounts -> MAX --------------------------- #


def _fake_headroom(monkeypatch, states):
    """Patch the batch headroom read with a per-account verdict map."""
    import fno.adapters.providers.runtime_state as rs
    from fno.adapters.providers.runtime_state import Headroom, HeadroomState

    def _many(provider_ids, **_kw):
        return {
            pid: Headroom(
                getattr(HeadroomState, states.get(pid, "unknown").upper()),
                None,
                source="lock",
            )
            for pid in provider_ids
        }

    monkeypatch.setattr(rs, "headrooms", _many)
    return _many


def _account_settings(*records: dict) -> object:
    """A settings stand-in declaring exactly these account records.

    Passing this is not decoration. ``settings=None`` means "load the real
    config" - correct in production, ambient in a test, and `load_settings` is
    `lru_cache`d per process, so whatever an earlier test in the same xdist
    worker put in that cache is what these assertions read. One extra
    claude-bound record is enough to flip the first assertion below, because
    the fake headroom answers `unknown` for any account it was not told about
    and `unknown` outranks `exhausted` in `_CAPACITY_RANK`. Measured: a lone
    `{"id": "ccm", "harness": "claude"}` turns `exhausted` into `unknown`.
    """
    return type("S", (), {"accounts": type("A", (), {"records": list(records)})})()


def test_runtime_capacity_aggregates_max_over_accounts(monkeypatch):
    """AC11-HP: an exhausted record bound to claude reads claude exhausted only
    when EVERY claude account is; one healthy account means usable."""
    inv = _inv([
        {"name": "opus-x", "harness": "claude", "model": "o", "band": "high",
         "account": "primary"},
    ])
    _fake_headroom(monkeypatch, {"primary": "exhausted"})
    cap = rr.runtime_capacity(("claude",), settings=_account_settings(), inventory=inv)
    # the harness has one declared account, exhausted -> exhausted
    assert cap["claude"]["state"] == "exhausted"
    # a second healthy account (registered record) makes the harness usable
    settings = _account_settings(
        {"id": "primary", "harness": "claude"},
        {"id": "backup", "harness": "claude"},
    )
    _fake_headroom(monkeypatch, {"primary": "exhausted", "backup": "ok"})
    cap = rr.runtime_capacity(("claude",), settings=settings, inventory=inv)
    assert cap["claude"]["state"] == "ok"
    assert cap["claude"]["accounts"] == {"primary": "exhausted", "backup": "ok"}
    # exhausted + UNKNOWN is NOT exhausted: exhaustion requires every account
    _fake_headroom(monkeypatch, {"primary": "exhausted"})
    cap = rr.runtime_capacity(("claude",), settings=settings, inventory=inv)
    assert cap["claude"]["state"] == "unknown"


def test_harness_accounts_expands_rows_then_registered_records(monkeypatch):
    class _Settings:
        class accounts:
            records = [{"id": "rec-a", "harness": "claude"}, {"id": "rec-b", "harness": "codex"}]
    inv = _inv([
        {"name": "opus-x", "harness": "claude", "model": "o", "route": "zai/glm-5.3"},
        {"name": "flash-x", "harness": "claude", "model": "f", "account": "paid-lane"},
    ])
    # a row's explicit account names its record; a route names a VENDOR, never
    # an account id, so it contributes nothing (a vendor key could never match
    # the account-keyed state and would dilute a live lock with its UNKNOWN)
    assert rr.harness_accounts("claude", settings=_Settings, inventory=inv) == [
        "paid-lane", "rec-a",
    ]
    # no declared row for codex -> every registered record bound to codex
    assert rr.harness_accounts("codex", settings=_Settings, inventory=inv) == ["rec-b"]


def test_same_model_two_access_paths_two_cost_profiles():
    """Cost belongs to the ACCESS PATH: the same model reached two ways is two
    rows with two cost profiles, and the cheaper path wins within the band -
    never an averaged number."""
    rows = [
        {"name": "flash-subscription", "harness": "claude",
         "model": "glm-5.3-flash", "band": "medium",
         "route": "zai/glm-5.3-flash", "cost_per_mtok_in": 2.3},
        {"name": "flash-api", "harness": "opencode",
         "model": "glm-5.3-flash", "band": "medium",
         "cost_per_mtok_in": 0.075},
    ]
    inv = _inv(rows)
    assert len(inv.rows) == 2  # both rows kept, nothing averaged or merged
    candidate, chain = _grid("medium", "p2", {"claude": "ok", "opencode": "ok"}, inv=inv)
    assert candidate["model"] == "glm-5.3-flash"
    assert candidate["harness"] == "opencode"  # the cheaper ACCESS PATH
    assert any("flash-api" in step for step in chain)
    # the expensive path still stands when the cheap one is exhausted
    candidate, _ = _grid("medium", "p2", {"claude": "ok", "opencode": "exhausted"}, inv=inv)
    assert candidate["harness"] == "claude"


def test_runtime_capacity_records_window_absent_with_no_accounts(monkeypatch):
    # "no accounts" has to be DECLARED. Left ambient this reads the real config
    # through the process-cached `load_settings`, and on a machine that has
    # claude account records it probes their real headroom and reports a window
    # of "lock" rather than "absent". Same defect as the aggregation test above.
    cap = rr.runtime_capacity(
        ("claude",), settings=_account_settings(), inventory=rr.Inventory()
    )
    assert cap["claude"]["window"] == "absent"
    assert cap["claude"]["state"] == "unknown"


def test_runtime_capacity_keeps_per_account_sources_and_observed_at(monkeypatch):
    """The payload keeps each account's provenance beside its state, so a
    named row never has to guess whose evidence the harness window named.
    The fields the Rust walk already reads stay byte-compatible."""
    import fno.adapters.providers.runtime_state as rs
    from fno.adapters.providers.runtime_state import Headroom, HeadroomState

    def _many(provider_ids, **_kw):
        return {
            "paid": Headroom(
                HeadroomState.EXHAUSTED, 123.0, source="lock", observed_at=1000.0
            ),
            "free": Headroom(
                HeadroomState.UNKNOWN, None, source="stale", observed_at=None
            ),
        }

    monkeypatch.setattr(rs, "headrooms", _many)
    inv = _inv([
        {"name": "x", "harness": "claude", "model": "o", "band": "high",
         "account": "paid"},
        {"name": "y", "harness": "claude", "model": "m", "band": "low",
         "account": "free"},
    ])
    cap = rr.runtime_capacity(("claude",), settings=_account_settings(), inventory=inv)
    assert cap["claude"]["sources"] == {"paid": "lock", "free": "stale"}
    assert cap["claude"]["observed_at"] == {"paid": 1000.0, "free": None}
    # untouched shapes: the harness-wide worst and the per-account states.
    # unknown outranks exhausted in _CAPACITY_RANK, so the worst account's
    # source (stale) is the harness window.
    assert cap["claude"]["accounts"] == {"paid": "exhausted", "free": "unknown"}
    assert cap["claude"]["window"] == "stale"


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
    assert model is None and source == "provider-default(no-difficulty)"


# --- the built-in fallback (config overrides AND extends it) ---------------- #


class _FakeRouting:
    def __init__(self, models, objective="cheapest-that-clears", prefer=""):
        self.models = models
        self.objective = objective
        self.prefer_harness = prefer


class _FakeSettings:
    def __init__(self, models, **kw):
        self.routing = _FakeRouting(models, **kw)


def _resolved(models):
    """resolve_inventory against a config declaring exactly ``models``."""
    return rr.resolve_inventory(settings=_FakeSettings(models), snapshot={})


def test_the_builtin_table_answers_when_config_declares_nothing():
    """The fallback keeps a tier request answerable. Without it, review level
    names no model and /code-review drops to the provider default on every
    install that has declared no inventory."""
    inv = _resolved([])
    assert inv.rows, "the built-in fallback must seed the inventory"
    assert inv.declared is False, "seeded rows are not a config declaration"
    model, _chain = rr.resolve_tier("high", settings=_FakeSettings([]), snapshot={})
    assert model is not None


def test_config_overrides_a_builtin_row_per_field():
    """A config row REPLACES the built-in of the same name field by field, and
    the fields it does not name keep the built-in value."""
    inv = _resolved([{"name": "glm-4.7", "model": "glm-4.7-pinned"}])
    row = inv.rows["glm-4.7"]
    assert row.model == "glm-4.7-pinned"
    assert row.harness == "claude", "an unnamed field keeps the built-in value"
    assert row.band == "low", "an unnamed field keeps the built-in value"
    assert inv.declared is True


def test_config_extends_the_builtin_table_with_a_new_name():
    """A name the built-in never carried is ADDED, never swapped in beside a
    table that then wins: adding a model stays a config edit."""
    inv = _resolved([
        {"name": "local-llama", "harness": "claude", "model": "llama-x", "band": "high"},
    ])
    assert "local-llama" in inv.rows, "config must extend the table"
    assert "glm-4.7" in inv.rows, "extending must not drop the built-in rows"


def test_the_grid_still_injects_nothing_when_config_declares_nothing():
    """The fallback seeds rows, so the grid reads `declared` rather than
    `rows`. A virgin install stays inert and says why."""
    candidate, chain = _grid("high", "p1", {"claude": "ok"}, inv=_resolved([])
    )
    assert candidate is None
    assert chain[-1] == "grid=no-inventory-declared"


def test_a_model_in_two_bands_keeps_the_strongest_whatever_the_table_order(monkeypatch):
    """The fallback bands `gpt-5.6-sol` from `max`, not from `high`.

    It sits in both. A regression guard on the rule, not the test that caught
    the bug - the duplicate-row test below is that one, and this assertion
    held under the defect too. Kept because the rule now comes from rank, so
    this also pins that the table's own order cannot decide a band.
    """
    from fno.adapters.providers import benchmarks as _bm

    assert "gpt-5.6-sol" in _bm.STATIC_TIERS["max"], "premise: listed in max"
    assert "gpt-5.6-sol" in _bm.STATIC_TIERS["high"], "premise: listed in high too"

    def _band_of(table):
        monkeypatch.setattr(_bm, "STATIC_TIERS", table)
        return {r["name"]: r["band"] for r in rr._builtin_rows()}["gpt-5.6-sol"]

    forward = dict(_bm.STATIC_TIERS)
    reversed_table = dict(reversed(list(_bm.STATIC_TIERS.items())))
    assert _band_of(forward) == "max"
    assert _band_of(reversed_table) == "max"


def test_the_fallback_emits_one_row_per_model():
    """One row per name is what leaves the fold no same-name order to depend
    on. A duplicate would reintroduce exactly the ordering bug above."""
    names = [r["name"] for r in rr._builtin_rows()]
    assert len(names) == len(set(names)), f"duplicate fallback rows: {names}"


# --- declared rows: the loader's real return type ---------------------------- #


def test_declared_rows_survive_the_real_loader(tmp_path):
    """The slot payload's declared rows must accept what the loader returns.

    The x-947c defect was a type test at the READER: an isinstance-Mapping
    filter dropped rows the loader had parsed fine, and grid-routed spawns
    were refused with 'declared rows: (none)'. Rows are therefore plain
    mappings handed to readers verbatim - the Mapping spelling is the
    contract. Drives the REAL loader (settings_from_files: the same parse +
    validate path as production) and asserts the named row survives - a
    nonzero count alone cannot tell a right answer from a silent empty.
    operator_view rides every declared row: the qualification field the
    strict inventory reads (x-90a9).
    """
    from fno.config import settings_from_files

    cfg = tmp_path / "config.toml"
    cfg.write_text(
        "[[routing.models]]\n"
        'name = "claude-opus-5"\n'
        'harness = "claude"\n'
        'model = "claude-opus-5"\n'
        'band = "high"\n'
        'operator_view = "claude-native"\n'
        "\n"
        "[[routing.models]]\n"
        'name = "zai-flash"\n'
        'harness = "claude"\n'
        'model = "glm-5.3-flash[1m]"\n'
        'band = "low"\n'
    )
    settings = settings_from_files([cfg])
    # premise, not decoration: the loader hands plain mappings - the spelling
    # every declared-row reader must consume without a type test.
    assert all(isinstance(r, dict) for r in settings.routing.models)

    rows = rr._declared_rows(settings)

    assert len(rows) == 2  # CONFIG-declared exactly, never the built-in fallback
    assert "claude-opus-5" in rows
    assert rows["claude-opus-5"]["model"] == "claude-opus-5"
    assert rows["claude-opus-5"]["harness"] == "claude"
    assert rows["claude-opus-5"]["operator_view"] == "claude-native"
    assert rows["zai-flash"]["band"] == "low"


def test_repeated_declared_name_folds_per_field(tmp_path):
    """A repeated name is a one-field override: unset fields keep the base
    row's values (the config model's own contract). A wholesale replace would
    re-home the lane to a default access path - a harness default or a lost
    vendor/account bills the wrong route."""
    from fno.config import settings_from_files

    cfg = tmp_path / "config.toml"
    cfg.write_text(
        "[[routing.models]]\n"
        'name = "dual-home"\n'
        'harness = "claude"\n'
        'model = "glm-5.3-flash[1m]"\n'
        'route = "zai/glm-5.3-flash[1m]"\n'
        'band = "low"\n'
        "\n"
        "[[routing.models]]\n"
        'name = "dual-home"\n'
        'model = "glm-5.3-flash"\n'
    )
    settings = settings_from_files([cfg])

    rows = rr._declared_rows(settings)

    assert len(rows) == 1
    row = rows["dual-home"]
    assert row["model"] == "glm-5.3-flash"  # the override wins on its field
    assert row["harness"] == "claude"  # unnamed fields keep the base row's value
    assert row["route"] == "zai/glm-5.3-flash[1m]"
    assert row["band"] == "low"


def test_routing_policy_payload_reads_the_opt_in_fields(tmp_path, monkeypatch):
    """The strict flag and the operator posture travel to the owner verbatim;
    an unset posture reads unknown, the conservative default."""
    import fno.config as config_mod

    f = tmp_path / "settings.toml"
    f.write_text(
        "[routing]\nenforce_inventory = true\noperator_access = \"remote\"\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(f))
    settings = config_mod.load_settings()
    assert rr._routing_policy_payload(settings) == {
        "enforce_inventory": True,
        "operator_access": "remote",
    }

    f2 = tmp_path / "unset.toml"
    f2.write_text("", encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(f2))
    settings2 = config_mod.load_settings()
    assert rr._routing_policy_payload(settings2)["operator_access"] == "unknown"
    assert rr._routing_policy_payload(settings2)["enforce_inventory"] is False
