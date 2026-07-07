"""Tests for dispatch-time model/tier resolution (the pareto router read side).

Covers the tier resolver's band logic + fallback chain (AC3-HP, AC3-FR), the
full precedence chain (AC2-EDGE: an explicit --model outranks a tier), the
static-table fallback when no snapshot exists, and the non-fatal node_model seam.
"""
from __future__ import annotations

from fno import route_resolve as rr


def _snap(models):
    return {"fetched_at": "2026-01-01T00:00:00+00:00", "source": "x", "models": models}


# --- resolve_tier ---------------------------------------------------------- #


def test_tier_picks_cheapest_that_clears_floor():
    """AC3-HP: low tier -> cheapest reachable model that clears the floor."""
    snap = _snap([
        {"name": "claude-opus-4-8", "coding_percentile": 99},
        {"name": "glm-4.7", "coding_percentile": 55},
    ])
    model, chain = rr.resolve_tier("low", snapshot=snap)
    assert model == "glm-4.7"  # cheapest (lowest pct) that clears floor 50
    assert any("glm-4.7" in step for step in chain)


def test_tier_high_empty_degrades_and_records_chain():
    """AC3-FR: no model clears the high floor -> degrade to best available, spawn."""
    snap = _snap([
        {"name": "glm-4.7", "coding_percentile": 55},
        {"name": "glm-5.2", "coding_percentile": 75},
    ])
    model, chain = rr.resolve_tier("high", snapshot=snap)
    assert model == "glm-5.2"  # best available below the floor
    assert any("degrade" in step for step in chain)


def test_tier_no_reachable_falls_to_provider_default():
    snap = _snap([{"name": "some-unmapped", "coding_percentile": 99}])
    model, chain = rr.resolve_tier("high", snapshot=snap)
    assert model is None
    assert any("provider default" in step for step in chain)


def test_tier_unknown_is_provider_default():
    model, chain = rr.resolve_tier("turbo", snapshot=_snap([]))
    assert model is None
    assert any("unknown-tier" in step for step in chain)


def test_tier_no_snapshot_uses_static_table():
    """No snapshot -> the curated static band, still deterministic and reachable."""
    model, chain = rr.resolve_tier("low", snapshot={})  # empty snapshot -> static
    assert model == "glm-4.7"  # STATIC_TIERS['low'][0], reachable
    assert any("static" in step for step in chain)


# --- resolve_dispatch_model (precedence) ----------------------------------- #


def test_explicit_outranks_everything():
    """AC2-EDGE: a dispatch-time --model wins over a task tier (and role routing)."""
    model, source, chain = rr.resolve_dispatch_model(
        explicit="pinned-x", task_tier="high", snapshot=_snap([])
    )
    assert (model, source) == ("pinned-x", "explicit")


def test_task_pin_outranks_task_tier():
    model, source, _ = rr.resolve_dispatch_model(
        task_model="task-x", task_tier="high", snapshot=_snap([])
    )
    assert (model, source) == ("task-x", "task-pin")


def test_task_tier_resolves_and_labels_source():
    snap = _snap([{"name": "glm-4.7", "coding_percentile": 55}])
    model, source, _ = rr.resolve_dispatch_model(task_tier="low", snapshot=snap)
    assert model == "glm-4.7"
    assert source == "task-tier(low)"


def test_plan_tier_is_lowest_priority_before_default():
    snap = _snap([{"name": "glm-4.7", "coding_percentile": 55}])
    model, source, _ = rr.resolve_dispatch_model(plan_tier="low", snapshot=snap)
    assert model == "glm-4.7" and source == "plan-tier(low)"


def test_nothing_set_is_provider_default():
    model, source, _ = rr.resolve_dispatch_model()
    assert model is None and source == "provider-default"


# --- node_model (spawn seam) ----------------------------------------------- #


def test_node_model_reads_pin_and_tier():
    assert rr.node_model({"model": "glm-5.2"}) == "glm-5.2"
    # tier with an (empty) injected snapshot -> deterministic static table
    assert rr.node_model({"model_tier": "low"}, snapshot={}) == "glm-4.7"
    assert rr.node_model({}) is None


def test_node_model_explicit_override_wins():
    assert rr.node_model({"model_tier": "low"}, explicit="cli-x", snapshot={}) == "cli-x"
