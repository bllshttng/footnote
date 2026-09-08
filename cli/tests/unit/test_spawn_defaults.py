"""US8 spawn-seam injector: config.agents.defaults -> argv (x-de9d).

Precedence explicit flag > config > builtin, resolved field-by-field. Provider
validated (exit 2 on a bad name); config-sourced effort degrades open on a
no-surface provider while an explicit --effort stays fail-closed downstream.
"""
from __future__ import annotations

import io
import json

import pytest

from fno.rust_binary import find_dev_binary

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


from fno.agents.spawn_defaults import inject_spawn_defaults, resolve_lane_vendor


class _Defaults:
    def __init__(self, provider="", model="", effort="", substrate="", permission_mode="",
                 route="", account="", pane_group="", lanes=None, on_exhausted="",
                 by_difficulty=None, on_low="prefer_healthy", on_unknown="allow",
                 harness=None, **extra):
        self.provider = provider
        self.model = model
        self.effort = effort
        self.substrate = substrate
        self.permission_mode = permission_mode
        self.route = route
        self.account = account
        self.pane_group = pane_group
        self.harness = harness
        # Lane dicts are raw (the schema keeps `lanes: Any`), so a lane-only
        # field like args rides through as an attribute, mirroring the seam's
        # getattr read.
        for k, v in extra.items():
            setattr(self, k, v)
        self.lanes = [
            _Defaults(**lane) if isinstance(lane, dict) else lane
            for lane in (lanes or [])
        ]
        self.on_exhausted = on_exhausted
        self.by_difficulty = by_difficulty or {}
        self.on_low = on_low
        self.on_unknown = on_unknown


class _Settings:
    def __init__(self, profiles=None, model_routing=None, max_lanes=None, **kw):
        # profiles: {verb: {field: value}} -> {verb: _Defaults}
        prof = {k: _Defaults(**v) for k, v in (profiles or {}).items()}
        self.agents = type(
            "A",
            (),
            {
                "defaults": _Defaults(**kw),
                "profiles": prof,
                "max_lanes": max_lanes or {},
            },
        )()
        # a real ModelRoutingBlock so resolve_route can resolve a lane.
        self.model_routing = model_routing


def _lane(harness: str, **fields: object) -> dict:
    """A lanes[] entry keyed by the AXIS the value actually is.

    The schema spells the harness axis ``provider``, matching its
    ``agents.defaults``/``agents.profiles`` siblings, but the value is a
    HARNESS and not a vendor. One adapter keeps every lane in this file reading
    in the right vocabulary.
    """
    return {"provider": harness, **fields}


def _inject(args, err=None, env=None, profiles=None, model_routing=None, **cfg):
    return inject_spawn_defaults(
        args,
        settings=_Settings(profiles=profiles, model_routing=model_routing, **cfg),
        stderr=err,
        env=env or {},
    )


@pytest.mark.parametrize(
    ("provider", "computed_dirs", "expected"),
    [
        ("codex", ["/tmp/state", "/tmp/claims-root"], True),
        ("codex", ["/tmp/state"], False),
        ("gemini", ["/tmp/state", "/tmp/claims-root"], None),
    ],
)
def test_claim_store_writable_is_tri_state(provider, computed_dirs, expected, monkeypatch):
    from pathlib import Path

    from fno.agents import mux_spawn

    monkeypatch.setattr(
        "fno.claims.io.global_claims_root", lambda: Path("/tmp")
    )
    monkeypatch.setattr(
        "fno.claims.io.claims_dir", lambda root=None: Path("/tmp/claims-root")
    )

    assert mux_spawn._claim_store_writable(provider, computed_dirs) is expected


def test_non_spawn_verb_untouched():
    assert _inject(["ask", "w", "hi"], provider="codex") == ["ask", "w", "hi"]


def test_all_unset_is_noop():
    assert _inject(["spawn", "--name", "w", "hi"]) == ["spawn", "--name", "w", "hi"]


def _declare_inventory(monkeypatch, rows, objective="cheapest-that-clears", prefer=""):
    """Pin a declared routing inventory for the grid path (no config on disk)."""
    from fno import route_resolve as rr

    inv = rr.inventory_from_rows(rows, objective=objective, prefer_harness=prefer)
    monkeypatch.setattr(rr, "resolve_inventory", lambda **_kw: inv)
    return inv


def _two_harness_rows():
    return [
        {"name": "opus-x", "harness": "claude", "model": "claude-opus-5", "band": "high"},
        {"name": "sol-x", "harness": "codex", "model": "gpt-5.6-sol", "band": "high"},
    ]


def test_difficulty_grid_precedes_defaults_when_capacity_is_known(monkeypatch):
    """AC3-HP: the grid supplies harness/model below profiles and above defaults."""
    _declare_inventory(monkeypatch, _two_harness_rows())
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": "exhausted", "codex": "ok"},
    )
    result = _inject(
        ["spawn", "--name", "w", "--node", "x-grid1", "hi"],
        model="default-model",
    )
    assert "--harness" in result and "codex" in result
    assert "--model" in result and "gpt-5.6-sol" in result
    assert "default-model" not in result


def test_stage_profile_model_occupies_model_axis(monkeypatch):
    """AC3-HP: a profile-sourced model remains authoritative over the grid; the
    stand-down is a named receipt entry, not a silence."""
    _declare_inventory(monkeypatch, _two_harness_rows())
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": "ok", "codex": "ok"},
    )
    err = io.StringIO()
    result = _inject(
        ["spawn", "--name", "w", "--node", "x-grid1", "/target x"],
        profiles={"target": {"model": "profile-model"}},
        model="default-model",
        err=err,
    )
    assert "profile-model" in result
    assert "--harness" not in result
    assert "grid=model-axis-occupied" in err.getvalue()


def test_profile_provider_pins_harness_grid_fills_model_and_effort(monkeypatch):
    """AC8-HP: `[agents.profiles.target] provider = "codex"` names the HARNESS
    axis only - the grid still supplies model and effort within codex."""
    rows = _two_harness_rows() + [
        {"name": "sol-x", "effort": "high"},
    ]
    _declare_inventory(monkeypatch, rows)
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": "ok", "codex": "ok"},
    )
    result = _inject(
        ["spawn", "--name", "w", "--node", "x-grid1", "/target x"],
        profiles={"target": {"provider": "codex"}},
    )
    assert "--harness" in result and result[result.index("--harness") + 1] == "codex"
    assert "--model" in result and result[result.index("--model") + 1] == "gpt-5.6-sol"
    assert "--effort" in result and result[result.index("--effort") + 1] == "high"


def test_pinned_substrate_filters_candidates_instead_of_cancelling(monkeypatch):
    """AC9-EDGE: `--substrate pane` is universal, so the grid still fires; a
    thread-only substrate narrows to thread-capable harnesses (claude) rather
    than cancelling the decision."""
    _declare_inventory(monkeypatch, _two_harness_rows())
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": "ok", "codex": "ok"},
    )
    # pane: universal -> grid fires exactly as without the flag
    result = _inject(
        ["spawn", "--name", "w", "--node", "x-grid1", "--substrate", "pane", "hi"],
        model="default-model",
    )
    assert "--harness" in result
    # bg/thread: only claude is thread-capable, so codex is filtered OUT and
    # claude is picked - the flag narrowed the set, it did not stand the grid down
    result = _inject(
        ["spawn", "--name", "w", "--node", "x-grid1", "--substrate", "bg", "hi"],
        model="default-model",
    )
    assert "--harness" in result and result[result.index("--harness") + 1] == "claude"
    assert "gpt-5.6-sol" not in result
    # a mapped permission-mode is claude-only off pane: codex filtered out
    result = _inject(
        ["spawn", "--name", "w", "--node", "x-grid1",
         "--permission-mode", "bypassPermissions", "hi"],
        model="default-model",
    )
    assert "--harness" in result and result[result.index("--harness") + 1] == "claude"


def test_grid_effort_yields_to_explicit_effort_flag(monkeypatch):
    """AC7-HP: an explicit --effort wins over the grid's effort coordinate."""
    rows = [
        {"name": "sol-x", "harness": "codex", "model": "gpt-5.6-sol",
         "band": "high", "effort": "high"},
    ]
    _declare_inventory(monkeypatch, rows)
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"codex": "ok"},
    )
    result = _inject(
        ["spawn", "--name", "w", "--node", "x-grid1", "--effort", "low", "hi"],
    )
    assert result.count("--effort") == 1
    assert result[result.index("--effort") + 1] == "low"
    assert "--model" in result and "gpt-5.6-sol" in result


def test_grid_route_rides_beside_the_model_it_belongs_to(monkeypatch):
    """AC1-HP (x-b545): the grid candidate's route injects --route next to
    --harness/--model, so the gate sees the vendor the grid picked."""
    _declare_inventory(monkeypatch, [
        {"name": "zai-flash", "harness": "claude", "model": "glm-5.3-flash[1m]",
         "band": "high", "route": "zai/glm-5.3-flash[1m]", "account": "zai-main"},
    ])
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": {"state": "ok", "accounts": {"zai-main": "ok"}}},
    )
    result = _inject(["spawn", "--name", "w", "--node", "x-route1", "hi"])
    assert "--route" in result
    assert result[result.index("--route") + 1] == "zai/glm-5.3-flash[1m]"
    assert "--account" in result
    assert result[result.index("--account") + 1] == "zai-main"
    assert result[result.index("--model") + 1] == "glm-5.3-flash[1m]"
    assert result[result.index("--harness") + 1] == "claude"


def test_grid_account_skips_on_a_non_claude_grid_harness(monkeypatch):
    """The grid's account is claude-bound at the spawn CLI: a codex row
    carrying one warns and skips instead of silently dropping the pin."""
    _declare_inventory(monkeypatch, [
        {"name": "sol-x", "harness": "codex", "model": "gpt-5.6-sol",
         "band": "high", "account": "zai-main"},
    ])
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"codex": "ok"},
    )
    err = io.StringIO()
    result = _inject(["spawn", "--name", "w", "--node", "x-acct1", "hi"], err=err)
    assert "--account" not in result
    assert "account skipped" in err.getvalue()


def test_grid_account_wins_over_the_config_default(monkeypatch):
    """The grid read the row account's capacity, so its account outranks a
    config-sourced agents.defaults.account: exactly one --account on argv."""
    _declare_inventory(monkeypatch, [
        {"name": "zai-flash", "harness": "claude", "model": "glm-5.3-flash[1m]",
         "band": "high", "route": "zai/glm-5.3-flash[1m]", "account": "zai-main"},
    ])
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": {"state": "ok", "accounts": {"zai-main": "ok"}}},
    )
    result = _inject(
        ["spawn", "--name", "w", "--node", "x-acct2", "hi"], account="ccm"
    )
    assert result.count("--account") == 1
    assert result[result.index("--account") + 1] == "zai-main"


def test_grid_routeless_row_injects_no_route(monkeypatch):
    """AC4-EDGE (x-b545): a routeless row (claude-canonical-*) produces argv
    unchanged from today - no lane selector."""
    _declare_inventory(monkeypatch, [
        {"name": "opus-x", "harness": "claude", "model": "claude-opus-5", "band": "high"},
    ])
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": "ok"},
    )
    result = _inject(["spawn", "--name", "w", "--node", "x-route2", "hi"])
    assert "--route" not in result
    assert "--model" in result and "claude-opus-5" in result


def test_inert_grid_says_why_in_the_receipt(monkeypatch):
    """AC4-EDGE at the seam: no declared inventory -> the receipt carries
    grid=no-inventory-declared instead of silence."""
    from fno import route_resolve as rr

    monkeypatch.setattr(
        rr, "resolve_inventory", lambda **_kw: rr.Inventory()
    )
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": "ok", "codex": "ok"},
    )
    err = io.StringIO()
    _inject(["spawn", "--name", "w", "--node", "x-grid1", "hi"], err=err)
    assert "grid=no-inventory-declared" in err.getvalue()


def test_crown_profile_key_reaches_non_verb_seeds():
    """AC15-HP: a seed with no leading slash-verb - every king seed - resolves
    the profile key `crown`, so [agents.profiles.crown] applies to crown spawns."""
    from fno.agents.spawn_defaults import _profile_key

    assert _profile_key("king: shrink the board") == "crown"
    assert _profile_key("") == "crown"
    assert _profile_key("/fno:target x") == "target"
    assert _profile_key("/absolute/path/to/thing") == "crown"


def test_crown_profile_injects_on_a_non_verb_seed():
    result = _inject(
        ["spawn", "--name", "k", "king: shrink the board"],
        profiles={"crown": {"model": "crown-model"}},
    )
    assert "crown-model" in result


def test_plan_presence_selects_planning_or_execution_band(monkeypatch):
    """AC13-HP: a /target on an unplanned node bills planning (band floored
    high); the same node WITH a plan_path bills execution (stamped band)."""
    rows = [
        {"name": "cheap-x", "harness": "codex", "model": "gpt-cheap", "band": "low"},
        {"name": "strong-x", "harness": "codex", "model": "gpt-strong", "band": "high"},
    ]
    _declare_inventory(monkeypatch, rows)
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": "ok", "codex": "ok"},
    )
    node = {"difficulty": "low", "priority": "p2"}
    planned = {"difficulty": "low", "priority": "p2", "plan_path": "/tmp/plan.md"}
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node", lambda *a, **k: dict(node)
    )
    out = _inject(["spawn", "--node", "x-1", "/target x-1"])
    assert "gpt-strong" in out and "gpt-cheap" not in out
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node", lambda *a, **k: dict(planned)
    )
    out = _inject(["spawn", "--node", "x-1", "/target x-1"])
    assert "gpt-cheap" in out and "gpt-strong" not in out


def test_ac3_bare_spawn_inherits_provider_and_model():
    # AC3-HP: bare spawn inherits both fields.
    out = _inject(["spawn", "--name", "w", "hi"], provider="codex", model="gpt-5.6-sol")
    assert out[0] == "spawn"
    assert "--harness" in out and out[out.index("--harness") + 1] == "codex"
    assert "--model" in out and out[out.index("--model") + 1] == "gpt-5.6-sol"
    # positionals preserved after the injected flags
    assert out[-2:] == ["w", "hi"]


def test_config_model_skipped_when_resolved_provider_differs():
    # The ambient config model applies only to the harness it was written for.
    # config provider=codex, model=gpt-5.6-sol, but -p claude retargets the
    # spawn: the codex model must NOT be forced onto a claude spawn (it would
    # 400 after the round-trip). explicit --model stays the supported override.
    err = io.StringIO()
    out = _inject(
        ["spawn", "-H", "claude", "--name", "w", "hi"],
        err=err,
        provider="codex",
        model="gpt-5.6-sol",
    )
    assert out.count("--harness") == 0  # no config harness injected (-H is explicit)
    assert "-H" in out  # the explicit flag survives
    assert out.count("--model") == 0  # codex model not forced onto claude
    msg = err.getvalue()
    assert "gpt-5.6-sol" in msg and "codex" in msg and "claude" in msg


def test_explicit_equals_form_wins():
    out = _inject(["spawn", "--model=mine", "w"], model="cfg")
    assert "cfg" not in out
    assert "--model=mine" in out


def test_ac4_bad_config_provider_exits_2():
    # AC4-ERR: unknown provider name fails closed at the seam.
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _inject(["spawn", "--name", "w", "hi"], err=err, provider="cluade")
    assert exc.value.code == 2
    assert "agents.defaults.provider" in err.getvalue()


def test_ac5_visibility_notice():
    # AC5-FR: config-sourced fields are echoed.
    err = io.StringIO()
    _inject(["spawn", "w"], err=err, provider="codex", model="m")
    msg = err.getvalue()
    assert "agents.defaults" in msg
    assert "provider" in msg and "model" in msg


def test_ac6_effort_degrades_open_on_gemini():
    # AC6-ERR: config effort on a no-surface provider -> skip + notice, no flag.
    err = io.StringIO()
    out = _inject(["spawn", "-H", "gemini", "w"], err=err, effort="high")
    assert "--effort" not in out  # not injected
    assert "effort skipped" in err.getvalue()
    assert "gemini" in err.getvalue()


def test_effort_injected_for_surface_provider():
    out = _inject(["spawn", "w"], provider="codex", effort="high")
    assert "--effort" in out and out[out.index("--effort") + 1] == "high"


def test_config_effort_forwards_provider_specific_value():
    # The provider/model owns the vocabulary, so a config-sourced value passes
    # through even when fno has no local catalog for it.
    err = io.StringIO()
    out = _inject(["spawn", "w"], err=err, provider="codex", effort="max")
    assert "--effort" in out
    assert out[out.index("--effort") + 1] == "max"
    assert "effort skipped" not in err.getvalue()


def test_config_effort_arbitrary_value_passes_through():
    # A provider/model-specific effort value never reaches a local allowlist.
    err = io.StringIO()
    out = _inject(["spawn", "w"], err=err, provider="claude", effort="banana")
    assert "--effort" in out
    assert out[out.index("--effort") + 1] == "banana"
    assert "effort skipped" not in err.getvalue()


def test_explicit_effort_never_overridden():
    # An explicit --effort is left alone (x-a0e0 fail-closed owns it downstream).
    err = io.StringIO()
    out = _inject(["spawn", "-H", "gemini", "--effort", "high", "w"], err=err, effort="low")
    assert out.count("--effort") == 1
    assert "low" not in out
    assert "effort skipped" not in err.getvalue()  # config path never ran


def test_argv_boundary_not_scanned():
    # A prompt token after --argv must never be read as our flag.
    out = _inject(
        ["spawn", "w", "--argv", "tool", "--model", "x"],
        model="cfg",
    )
    # --model inside the payload does NOT count as present -> config injects.
    assert out.index("--model") < out.index("--argv")
    assert out[out.index("--model") + 1] == "cfg"
    # payload survives verbatim
    assert out[-3:] == ["tool", "--model", "x"]


def test_passthrough_fence_not_scanned():
    # x-1caa: a provider flag after a bare `--` fence is not fno's flag (same
    # contract as the --argv payload), so the config default still injects -
    # and injects BEFORE the fence, never into the passthrough tail.
    out = _inject(["spawn", "w", "--", "--model", "x"], model="cfg")
    assert out.index("--model") < out.index("--")
    assert out[out.index("--model") + 1] == "cfg"
    assert out[-3:] == ["--", "--model", "x"]


def test_permission_mode_fence_not_scanned():
    # x-1caa: a fenced --permission-mode is the PROVIDER's flag, so it must not
    # suppress the config permission default (the same suppression shape the
    # name-mint head scan fixes for --name).
    out = _inject(["spawn", "hi", "--", "--permission-mode", "plan"],
                  permission_mode="acceptEdits")
    assert out.index("--permission-mode") < out.index("--")
    assert out[out.index("--permission-mode") + 1] == "acceptEdits"


def test_profile_seed_survives_a_passthrough_fence():
    # x-1caa: the seed is the pre-fence MESSAGE; reading the first fenced token
    # instead silently dropped the profile layer for every passthrough spawn.
    out = _inject(
        ["spawn", "/review the PR", "--", "--verbose"],
        profiles={"review": {"model": "m2"}},
    )
    assert out.index("--model") < out.index("--")
    assert out[out.index("--model") + 1] == "m2"


def test_execute_seed_accepts_legacy_do_profile():
    out = _inject(
        ["spawn", "/fno:execute plan.md"],
        profiles={"do": {"model": "legacy-model"}},
    )
    assert out[out.index("--model") + 1] == "legacy-model"


def test_do_shim_seed_uses_execute_profile():
    out = _inject(
        ["spawn", "/fno:do plan.md"],
        profiles={"execute": {"model": "execute-model"}},
    )
    assert out[out.index("--model") + 1] == "execute-model"


def test_config_default_substrate_refuses_passthrough_after_injection():
    # x-1caa AC7: a substrate that arrives by CONFIG default reroutes to the
    # Rust lane before the Python CLI's own refusal can run, so the gate
    # re-runs on the post-injection argv at the seam.
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _inject(["spawn", "hi", "--", "--verbose"], substrate="headless", err=err)
    assert exc.value.code == 2
    assert "pane-only" in err.getvalue()


def test_value_flag_value_not_misread_as_our_flag():
    # `--cwd --model` -> "--model" is the cwd VALUE, not a model flag; config injects.
    out = _inject(["spawn", "w", "--cwd", "--model"], model="cfg")
    assert "cfg" in out


def test_effort_effective_provider_from_config():
    # No explicit -p; config provider decides the effort surface (codex has one).
    out = _inject(["spawn", "w"], provider="codex", effort="high")
    assert "--effort" in out


def test_help_never_errors_under_bad_config():
    # `spawn --help` must render help, not exit 2, even with a broken config.
    err = io.StringIO()
    out = _inject(["spawn", "--help"], err=err, provider="cluade")
    assert out == ["spawn", "--help"]  # untouched, no SystemExit
    assert err.getvalue() == ""


def test_help_after_argv_still_injects():
    # A --help inside the --argv payload is not a help request for spawn itself.
    out = _inject(["spawn", "w", "--argv", "tool", "--help"], provider="codex")
    assert "--harness" in out and out.index("--harness") < out.index("--argv")


def test_ac2_hp_codex_spawn_does_not_inherit_claude_model():
    # config model=opus (a claude alias), provider unset; an explicit -p codex
    # retargets the spawn. The claude model must NOT ride onto codex, and a
    # stderr line names the config model, its implied provider, and the resolved
    # one. env={} => resolve_dispatch_harness infers claude as the implied.
    err = io.StringIO()
    out = _inject(["spawn", "-H", "codex", "w"], err=err, env={}, model="opus")
    assert out.count("--model") == 0  # no --model opus injected
    assert "opus" not in out
    msg = err.getvalue()
    assert "opus" in msg and "codex" in msg  # names the model and resolved provider


def test_ac5_fr_provider_resolution_failure_degrades_open(monkeypatch):
    # If resolve_dispatch_harness raises, the model default must degrade to
    # injecting nothing rather than aborting the spawn.
    import fno.dispatch_flags as pr

    def _boom(*_a, **_k):
        raise RuntimeError("resolution exploded")

    monkeypatch.setattr(pr, "resolve_dispatch_harness", _boom)
    err = io.StringIO()
    # provider unset so the model branch must call resolve_dispatch_harness.
    out = _inject(["spawn", "-H", "codex", "w"], err=err, env={}, model="opus")
    assert out.count("--model") == 0  # nothing injected
    assert out[-1] == "w"  # spawn not aborted; positional preserved
    assert "resolution" in err.getvalue().lower() or "leaving" in err.getvalue().lower()


def test_ac6_edge_no_explicit_provider_injects_model_unchanged():
    # No explicit -p, config model=opus, provider unset: --model opus is injected
    # exactly as before, with no NEW skip/leave reason line. env={} => implied
    # provider (claude) == resolved provider (claude) => inject.
    err = io.StringIO()
    out = _inject(["spawn", "w"], err=err, env={}, model="opus")
    assert out[out.index("--model") + 1] == "opus"
    assert "--name" in out and out[-1] == "w"  # slug minted; "w" is the message
    # the "leaving model to the harness" skip line must NOT fire here
    assert "leaving model to the harness" not in err.getvalue()


def test_residual_ambient_codex_leaves_claude_model_to_harness():
    # x-0e29: no explicit -p, provider unset, but a CODEX-ambient marker. The
    # provider-less claude-shaped model (opus) must NOT ride onto the inferred
    # codex spawn (it 400s after the round-trip). home=claude != target=codex.
    err = io.StringIO()
    out = _inject(["spawn", "w"], err=err, env={"CODEX_THREAD_ID": "x"}, model="opus")
    assert out[:2] == ["spawn", "--name"] and out[3:] == ["w"]  # no --model
    assert "--model" not in out and "opus" not in out
    msg = err.getvalue()
    # the leave reason names the model, the scope (claude), and the target (codex)
    assert "opus" in msg and "claude" in msg and "codex" in msg
    assert "leaving model to the harness" in msg


def test_ambient_codex_with_matching_provider_still_injects():
    # A codex-primary user who BINDS the model (provider=codex) keeps injection
    # under a codex-ambient session: home=codex == target=codex.
    out = _inject(
        ["spawn", "w"], env={"CODEX_THREAD_ID": "x"},
        provider="codex", model="gpt-5-codex",
    )
    assert "--model" in out and out[out.index("--model") + 1] == "gpt-5-codex"


# --------------------------------------------------------------------------- #
# Per-verb profiles (x-3d5b)
# --------------------------------------------------------------------------- #

def test_ac1_hp_profile_field_injected_by_verb_key():
    # AC1-HP: profile model + defaults effort, provenance names each rung.
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "worker1", "/blueprint x-1234"], err=err,
        provider="claude", effort="high",
        profiles={"blueprint": {"model": "fable"}},
    )
    assert "--model" in out and out[out.index("--model") + 1] == "fable"
    assert "--effort" in out and out[out.index("--effort") + 1] == "high"
    msg = err.getvalue()
    assert "model=fable (agents.profiles.blueprint.model)" in msg
    assert "effort=high (agents.defaults.effort)" in msg


def test_ac2_hp_substrate_and_permission_from_profile():
    out = _inject(
        ["spawn", "--name", "w", "/target x-9"],
        provider="claude",
        profiles={"target": {"substrate": "bg", "permission_mode": "yolo"}},
    )
    assert "--substrate" in out and out[out.index("--substrate") + 1] == "bg"
    assert "--permission-mode" in out and out[out.index("--permission-mode") + 1] == "yolo"


def test_ac2_hp_explicit_substrate_token_wins_permission_still_injects():
    # A trailing `pane` token pins substrate (normalized to --substrate pane);
    # only permission-mode is injected from the profile.
    out = _inject(
        ["spawn", "--name", "w", "/target x-9", "pane"],
        provider="claude",
        profiles={"target": {"substrate": "bg", "permission_mode": "yolo"}},
    )
    assert out[out.index("--substrate") + 1] == "pane"
    assert out.count("--substrate") == 1
    assert "--permission-mode" in out and out[out.index("--permission-mode") + 1] == "yolo"


def test_ac3_hp_namespace_stripped_key():
    # /fno:think fires the think profile identically to /think.
    for seed in ("/think x", "/fno:think x"):
        out = _inject(
            ["spawn", "--name", "w", seed], provider="claude",
            profiles={"think": {"model": "fable"}},
        )
        assert out[out.index("--model") + 1] == "fable", seed


@requires_rust
def test_profile_lanes_walk_in_declared_order(monkeypatch):
    """The lanes list IS the rank: lane 0 is tried first on every spawn, and
    the live row count plays no part in where the walk starts."""
    import fno.agents.spawn_defaults as spawn_defaults

    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    lanes = [
        _lane("codex", effort="high", substrate="pane", permission_mode="yolo"),
        _lane("claude", route="zai/glm-5.3[1m]", substrate="bg"),
    ]
    for live_count in (0, 1, 2, 3):
        monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda n=live_count: [object()] * n)
        err = io.StringIO()
        out = _inject(
            ["spawn", "--name", f"w{live_count}", "/fno:target x-1"],
            err=err,
            profiles={"target": {"lanes": lanes}},
        )
        assert out[out.index("--harness") + 1] == "codex"
        assert "agents.profiles.target.lanes[0]" in err.getvalue()


@requires_rust
def test_profile_lanes_skip_capped_vendor(monkeypatch):
    import fno.agents.spawn_defaults as spawn_defaults
    import fno.agents.spawn_gate as spawn_gate

    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [object()])
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    monkeypatch.setattr(spawn_gate, "provider_live_count", lambda vendor: 2)
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"],
        err=err,
        max_lanes={"zai": 2},
        profiles={"target": {"lanes": [
            _lane("claude", route="zai/glm-5.3[1m]", substrate="bg"),
            _lane("codex", permission_mode="yolo"),
        ]}},
    )
    assert out[out.index("--harness") + 1] == "codex"
    assert "provider zai at 2 of 2" in err.getvalue()
    assert "agents.profiles.target.lanes[1]" in err.getvalue()


@requires_rust
def test_profile_only_lane_at_cap_refuses(monkeypatch):
    import fno.agents.spawn_defaults as spawn_defaults
    import fno.agents.spawn_gate as spawn_gate

    # The hermetic suite sets FNO_SPAWN_GATE=0 (hermetic.py), and that escape's
    # contract is that it never BLOCKS a spawn - so it disables exactly the
    # refusal under test here. Opt back in, or this asserts nothing.
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [])
    monkeypatch.setattr(spawn_gate, "provider_live_count", lambda vendor: 2)
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _inject(
            ["spawn", "--name", "w", "/fno:target x-1"],
            err=err,
            max_lanes={"zai": 2},
            profiles={"target": {"lanes": [
                _lane("claude", route="zai/glm-5.3[1m]", substrate="bg"),
            ]}},
        )
    assert exc.value.code == 2
    assert "zai" in err.getvalue() and "2 of 2" in err.getvalue()


@requires_rust
def test_profile_capped_lane_refuses_when_count_unavailable(monkeypatch):
    import fno.agents.spawn_defaults as spawn_defaults
    import fno.agents.spawn_gate as spawn_gate

    # The hermetic suite sets FNO_SPAWN_GATE=0 (hermetic.py), and that escape's
    # contract is that it never BLOCKS a spawn - so it disables exactly the
    # refusal under test here. Opt back in, or this asserts nothing.
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [])
    monkeypatch.setattr(
        spawn_gate,
        "provider_live_count",
        lambda vendor: (_ for _ in ()).throw(spawn_gate.ProviderCountUnavailable("registry incomplete")),
    )
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _inject(
            ["spawn", "--name", "w", "/fno:target x-1"],
            err=err,
            max_lanes={"zai": 2},
            profiles={"target": {"lanes": [
                _lane("claude", route="zai/glm-5.3[1m]"),
            ]}},
        )
    assert exc.value.code == 2
    assert "registry incomplete" in err.getvalue()


@requires_rust
def test_profile_lane_unknown_harness_refuses(monkeypatch):
    import fno.agents.spawn_defaults as spawn_defaults

    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [])
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _inject(
            ["spawn", "--name", "w", "/fno:target x-1"],
            err=err,
            profiles={"target": {"lanes": [_lane("banana")]}},
        )
    assert exc.value.code == 2
    assert "agents.profiles.target.lanes[0].provider" in err.getvalue()


@requires_rust
@requires_rust
def test_profile_lane_injects_pane_group(monkeypatch):
    import fno.agents.spawn_defaults as spawn_defaults

    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [])
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"],
        err=err,
        profiles={"target": {"lanes": [{
            "provider": "codex",
            "substrate": "pane",
            "permission_mode": "yolo",
            "pane_group": "codex",
        }]}},
    )
    assert out[out.index("--tab") + 1] == "codex"
    assert "agents.profiles.target.lanes[0].pane_group" in err.getvalue()


def test_explicit_tab_wins_over_profile_pane_group(monkeypatch):
    import fno.agents.spawn_defaults as spawn_defaults

    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [])
    out = _inject(
        ["spawn", "--name", "w", "--tab", "name:manual", "/fno:target x-1"],
        profiles={"target": {"lanes": [{
            "provider": "codex",
            "substrate": "pane",
            "permission_mode": "yolo",
            "pane_group": "codex",
        }]}},
    )
    assert out.count("--tab") == 1
    assert out[out.index("--tab") + 1] == "name:manual"


def test_ac4_err_incompatible_config_substrate_degrades_open():
    # bg on a codex-resolved spawn is now the earned persistent thread lane.
    err = io.StringIO()
    out = _inject(
        ["spawn", "-H", "codex", "--name", "w", "/think x"], err=err,
        profiles={"think": {"substrate": "bg"}},
    )
    assert out[out.index("--substrate") + 1] == "bg"
    msg = err.getvalue()
    assert "substrate skipped" not in msg


def test_ac5_err_unknown_profile_provider_fails_closed():
    # AC5-ERR: matched profile with a bad provider exits 2 naming the rung.
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _inject(
            ["spawn", "--name", "w", "/target x-1"], err=err,
            profiles={"target": _lane("banana")},
        )
    assert exc.value.code == 2
    assert "agents.profiles.target.provider" in err.getvalue()


def test_ac5_err_nonmatching_seed_spawns_normally_under_bad_profile():
    # The same bad-provider profile does NOT fire for a /think seed. The
    # verb-seeded built-in permission rung (x-7198) still fires independently
    # of any profile - a /think seed with no permission config resolves the
    # built-in exactly like any other verb-seeded spawn.
    out = _inject(
        ["spawn", "--name", "w", "/think x"],
        profiles={"target": _lane("banana")},
    )
    assert out == [
        "spawn",
        "--permission-mode",
        "bypassPermissions",
        "--name",
        "w",
        "/think x",
    ]


def test_ac6_edge_verb_not_first_token_no_profile():
    # AC6-EDGE: verb not first -> no key; only defaults inject.
    out = _inject(
        ["spawn", "--name", "w", "fix the /target docs"],
        provider="claude",
        profiles={"target": {"model": "opus"}},
    )
    assert "--model" not in out  # target profile never fired
    assert "--harness" in out  # defaults still applied


def test_ac6_edge_absolute_path_never_matches():
    out = _inject(
        ["spawn", "--name", "w", "/usr/bin/x is a path"],
        profiles={"usr": {"model": "opus"}},
    )
    assert "--model" not in out


def test_ac7_edge_explicit_flag_beats_profile_beats_defaults():
    # Explicit -m wins; without it, profile beats defaults.
    out1 = _inject(
        ["spawn", "-m", "haiku", "--name", "w", "/target x-1"],
        model="sonnet", profiles={"target": {"model": "opus"}},
    )
    assert out1.count("--model") == 0  # only the explicit -m
    assert "opus" not in out1 and "sonnet" not in out1

    out2 = _inject(
        ["spawn", "--name", "w", "/target x-1"],
        model="sonnet", profiles={"target": {"model": "opus"}},
    )
    assert out2[out2.index("--model") + 1] == "opus"


def test_uppercase_verb_no_key():
    # Deliberate: the verb surface is lowercase; /Target does not match.
    out = _inject(
        ["spawn", "--name", "w", "/Target x-1"],
        profiles={"target": {"model": "opus"}},
    )
    assert "--model" not in out


def test_message_via_flag_keys_profile():
    # The seed can arrive via --message rather than a positional.
    out = _inject(
        ["spawn", "w", "--message", "/blueprint x"],
        profiles={"blueprint": {"model": "fable"}},
    )
    assert out[out.index("--model") + 1] == "fable"


def test_ac9_ui_no_config_field_prints_no_applied_line():
    # A prose-seeded spawn with zero injected fields prints no `applied` line
    # (a verb-seeded spawn always resolves the built-in permission rung,
    # x-7198 - see test_ac9_ui_verb_seed_still_gets_the_builtin_applied_line).
    err = io.StringIO()
    _inject(["spawn", "--name", "w", "start the thing"], err=err, profiles={"other": {"model": "x"}})
    assert "applied" not in err.getvalue()


def test_ac9_ui_verb_seed_still_gets_the_builtin_applied_line():
    # x-7198: a verb-seeded spawn with no permission config resolves the
    # built-in bypassPermissions rung and DOES print an applied line, even
    # though no other field was injected.
    err = io.StringIO()
    _inject(["spawn", "--name", "w", "/target x"], err=err, profiles={"other": {"model": "x"}})
    assert "applied permission_mode=bypassPermissions" in err.getvalue()


def test_apply_permission_builtin_false_suppresses_the_rung():
    # x-7198 regression: retask.resolve_target_coordinate probes a candidate
    # relaunch with the same verb-seeded shape a real dispatch uses, but it
    # never launches anything - it diffs against an already-live worker. The
    # builtin must not fire there (apply_permission_builtin=False), or every
    # probe reads as "an explicit permission-mode override" and forces a
    # respawn the live worker never needed.
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/target x"],
        settings=_Settings(),
        apply_permission_builtin=False,
    )
    assert "--permission-mode" not in out
    # The default (True) still fires for every other caller, unchanged.
    out_default = inject_spawn_defaults(
        ["spawn", "--name", "w", "/target x"], settings=_Settings()
    )
    assert out_default[out_default.index("--permission-mode") + 1] == "bypassPermissions"


def test_unknown_config_substrate_degrades_open():
    # An unknown substrate value is never injected (it would exit 2 at the spawn
    # parser); it degrades open with an "unknown substrate" warning.
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/target x"], err=err, provider="claude",
        profiles={"target": {"substrate": "banana"}},
    )
    assert "--substrate" not in out
    assert "unknown substrate" in err.getvalue()


def test_permission_mode_skipped_on_nonclaude_headless():
    # codex headless cannot honor a mapped --permission-mode (its one-shot lane
    # hardcodes its own bypass and exits 2); the config value degrades open.
    err = io.StringIO()
    out = _inject(
        ["spawn", "-H", "codex", "--headless", "--name", "w", "/target x"], err=err,
        profiles={"target": {"permission_mode": "yolo"}},
    )
    assert "--permission-mode" not in out
    assert "permission-mode skipped" in err.getvalue()


def test_permission_mode_ok_on_nonclaude_pane():
    # The pane lane maps every provider, so codex+pane honors a mapped value.
    out = _inject(
        ["spawn", "-H", "codex", "--name", "w", "/target x", "pane"],
        profiles={"target": {"permission_mode": "yolo"}},
    )
    assert out[out.index("--permission-mode") + 1] == "yolo"


def test_permission_mode_injected_on_bare_nonclaude_spawn_pane_default():
    # No explicit substrate: `fno agents spawn` defaults to PANE (not the
    # autonomous headless default), which maps codex permission modes - so the
    # configured value must be injected, not skipped as incompatible.
    out = _inject(
        ["spawn", "-H", "codex", "--name", "w", "/target x"],
        profiles={"target": {"permission_mode": "yolo"}},
    )
    assert out[out.index("--permission-mode") + 1] == "yolo"


def test_explicit_yolo_suppresses_config_permission_mode():
    # --yolo/-Y is the same knob as --permission-mode (mutually exclusive
    # downstream); an explicit yolo must win, so no config value is injected.
    for flag in ("--yolo", "-Y"):
        out = _inject(
            ["spawn", "--name", flag, "/target x"], provider="claude",
            profiles={"target": {"permission_mode": "bypassPermissions"}},
        )
        assert "--permission-mode" not in out, flag


def test_only_harness_flags_feed_the_provider_aware_default_scan():
    """The default scan resolves the HARNESS, so only --harness/-H may feed it.
    --provider names the model vendor: reading it as a harness would make an
    ambient claude-only default (bg) skip itself on a routed claude spawn."""
    for flag in ("--harness", "-H"):
        out = _inject(["spawn", "hi", flag, "codex"], substrate="bg")
        assert out[out.index("--substrate") + 1] == "bg", flag

    # --provider zai leaves the harness unresolved (claude by default), so the
    # claude-only bg default still applies.
    out = _inject(["spawn", "--name", "w", "hi", "--provider", "zai", "--model", "glm-5.2"],
                  substrate="bg")
    assert out[out.index("--substrate") + 1] == "bg"

    out = _inject(["spawn", "--name", "w", "hi", "--harness", "claude"], substrate="pane")
    assert out[out.index("--substrate") + 1] == "pane"


# --------------------------------------------------------------------------- #
# Role-aware model injection
#
# inject_spawn_defaults was role-blind: --role appeared only in the value-flag
# skip list, never in the routing decision, so a spawn carrying --role build
# (resolved to zai/glm-5.2[1m] via env) still got --model opus injected from
# config.agents.defaults.model. The CLI flag and the routed env collided; the
# worker was believed to be on the cheap lane and billed on the expensive one.
# A spawn whose --role resolves to a real route must not receive the config
# model: the route owns the model. A bare spawn still inherits the default.
# --------------------------------------------------------------------------- #

def _build_routing():
    from fno.config import ModelRoutingBlock

    return ModelRoutingBlock(roles={"build": "zai/glm-5.2[1m]"})


def test_role_with_resolved_route_skips_config_model():
    # The live bug: --role build resolves (zai configured + key present), so the
    # config model opus must NOT be injected. The route owns the model via env.
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "--role", "build", "/fno:target x-1"],
        err=err, env={"ZAI_API_KEY": "k"}, model="opus", model_routing=_build_routing(),
    )
    assert "--model" not in out
    assert "opus" not in out
    msg = err.getvalue()
    assert "build" in msg  # the notice names the role whose route owns the model


def test_role_without_route_still_inherits_config_model():
    # --role present but the lane does NOT resolve (no key -> fail-safe to the
    # primary model), so the config default applies exactly as a bare spawn.
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "--role", "build", "/fno:target x-1"],
        err=err, env={}, model="opus", model_routing=_build_routing(),
    )
    assert out[out.index("--model") + 1] == "opus"


def test_bare_spawn_still_inherits_default_model_unaffected_by_role_fix():
    # No --role: the default model is injected exactly as before the fix.
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"], err=err, env={}, model="opus",
    )
    assert out[out.index("--model") + 1] == "opus"


def test_explicit_model_wins_over_role_route():
    # An explicit -m is the supported cross-harness override; the role fix must
    # not change that, nor re-inject the config model alongside it.
    err = io.StringIO()
    out = _inject(
        ["spawn", "-m", "sonnet", "--name", "w", "--role", "build", "/fno:target x-1"],
        err=err, env={"ZAI_API_KEY": "k"}, model="opus", model_routing=_build_routing(),
    )
    assert out.count("--model") == 0  # only the explicit -m survives
    assert "opus" not in out


# --------------------------------------------------------------------------- #
# route / account fields beside the legacy provider (ruling 4)
#
# provider keeps meaning harness (-H); route carries vendor/model as
# vendor/model, forwarded as --route (fail-closed downstream on an unknown
# vendor or a missing key); account forwards --account. The names carry no axis
# word, so the four-axis guard never reads them as bindings. A config route
# owns the model, so the config model is not injected alongside it.
# --------------------------------------------------------------------------- #

# A harness literal held under a non-axis binding name, so the four-axis guard
# does not read it as a provider-named/harness-literal collision (a combination
# test needs provider + route together, and the baseline counts literal hits).
_CLAUDE = "claude"


def test_route_field_injected_as_flag():
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"], err=err, route="zai/glm-5.2[1m]",
    )
    assert out[out.index("--route") + 1] == "zai/glm-5.2[1m]"
    msg = err.getvalue()
    assert "route=zai/glm-5.2[1m] (agents.defaults.route)" in msg  # axis, value, source


def test_account_field_injected_as_flag():
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"], err=err, account="secondary",
    )
    assert out[out.index("--account") + 1] == "secondary"


def test_provider_and_route_both_injected_on_the_right_axes():
    # The verify case: provider (harness) and route (vendor/model) emit two
    # independent flags; vendor and model land on the right axes because route
    # is position-carried. route owns the model, so no config --model rides along.
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"], err=err,
        provider=_CLAUDE, route="zai/glm-5.2[1m]", model="opus",
    )
    assert out[out.index("--harness") + 1] == "claude"
    assert out[out.index("--route") + 1] == "zai/glm-5.2[1m]"
    assert "--model" not in out  # route owns the model
    assert "opus" not in out


def test_route_and_account_both_injected():
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"],
        route="zai/glm-5.2[1m]", account="secondary",
    )
    assert out[out.index("--route") + 1] == "zai/glm-5.2[1m]"
    assert out[out.index("--account") + 1] == "secondary"


def test_explicit_route_wins_over_config_route():
    out = _inject(
        ["spawn", "--name", "w", "--route", "explicit/m", "/fno:target x-1"],
        route="zai/glm-5.2[1m]",
    )
    assert out.count("--route") == 1
    assert out[out.index("--route") + 1] == "explicit/m"


def test_explicit_account_wins_over_config_account():
    out = _inject(
        ["spawn", "--name", "w", "--account", "explicit", "/fno:target x-1"],
        account="secondary",
    )
    assert out.count("--account") == 1
    assert out[out.index("--account") + 1] == "explicit"


def test_explicit_vendor_and_model_spelling_wins_over_config_route():
    # -P <vendor> -m <model> carries the same two pieces of information as
    # --route vendor/model. cmd_spawn rejects two route spellings together, so
    # injecting the config route on top of this explicit pair would abort a
    # spawn that already named its route, just spelled differently.
    out = _inject(
        ["spawn", "--name", "w", "-P", "zai", "-m", "glm-5.2[1m]", "/fno:target x-1"],
        route="zai/glm-5.2[1m]",
    )
    assert "--route" not in out
    assert out[out.index("-P") + 1] == "zai"
    assert out[out.index("-m") + 1] == "glm-5.2[1m]"


def test_bare_explicit_model_wins_over_config_route():
    # A bare -m (no -P) already names the model half of a route. cmd_spawn does
    # not reject --route alongside a bare -m the way it rejects -P+-m against
    # --route (no such check exists in cmd_spawn), so this collision would
    # previously slip through here: --route got injected alongside the
    # explicit -m, landing the spawn on the routed vendor's endpoint while
    # still asking for the explicit (unrelated) model - the exact
    # invisible-billing shape this field exists to kill.
    out = _inject(
        ["spawn", "-m", "sonnet", "--name", "w", "/fno:target x-1"],
        route="zai/glm-5.2[1m]",
    )
    assert "--route" not in out
    assert out[out.index("-m") + 1] == "sonnet"


def test_bare_explicit_vendor_wins_over_config_route():
    # A bare -P (no -m) already names the vendor half of a route. cmd_spawn
    # rejects vendor + --route together ("two spellings of one route") before
    # its own "add --model" check, so injecting a config route here would turn
    # a helpful "add --model" error into a confusing route-collision one on an
    # argv the operator never paired with a route at all.
    out = _inject(
        ["spawn", "-P", "zai", "--name", "w", "/fno:target x-1"],
        route="zai/glm-5.2[1m]",
    )
    assert "--route" not in out
    assert out[out.index("-P") + 1] == "zai"


def test_glued_short_vendor_flag_wins_over_config_route():
    # typer/click accepts the glued short-option form -Pzai for -P (a value
    # option), equivalent to -P zai. The vendor-detection scan must recognize
    # it too, or a config route still injects alongside an operator's already-
    # pinned vendor - the same collision the spaced -P zai form is guarded
    # against just above.
    out = _inject(
        ["spawn", "-Pzai", "--name", "w", "/fno:target x-1"],
        route="zai/glm-5.2[1m]",
    )
    assert "--route" not in out
    assert "-Pzai" in out


def test_flag_scan_does_not_misread_another_flags_consumed_value():
    # A literal "--route" that is --session-id's VALUE (not a real --route
    # flag) must not be misread as an explicit route: the config route still
    # injects, since the caller never actually passed --route.
    out = _inject(
        ["spawn", "--session-id", "--route", "--name", "w", "/fno:target x-1"],
        route="zai/glm-5.2[1m]",
    )
    assert "zai/glm-5.2[1m]" in out


def test_bare_explicit_vendor_suppresses_config_model():
    # The model-path twin of test_bare_explicit_vendor_wins_over_config_route:
    # a bare -P (no -m) must not receive an injected config model either, or
    # the result pairs a config model with a DIFFERENT vendor - e.g. -P zai +
    # injected --model opus -> route "zai/opus", an anthropic model at a zai
    # endpoint. The exact invisible-billing shape this whole module exists to
    # kill, previously reachable via the model path even though the route path
    # was already guarded.
    out = _inject(
        ["spawn", "-P", "zai", "--name", "w", "/fno:target x-1"],
        model="opus",
    )
    assert "--model" not in out
    assert out[out.index("-P") + 1] == "zai"


def test_config_account_not_injected_over_explicit_non_claude_harness():
    # Accounts are Claude-only; cmd_spawn rejects --account on any other
    # harness. A configured account must not follow an explicit -H codex (e.g.
    # an autonomous Claude-to-Codex quota cutover), or the cutover aborts.
    out = _inject(
        ["spawn", "--name", "w", "-H", "codex", "/fno:target x-1"],
        account="secondary",
    )
    assert "--account" not in out


def test_config_account_skip_is_not_silent():
    # AC9-UI: config-sourced routing is never invisible. The substrate/
    # permission_mode skip paths already warn to stderr; the account skip on a
    # non-claude harness must too, not silently drop the pin.
    err = io.StringIO()
    _inject(
        ["spawn", "--name", "w", "-H", "codex", "/fno:target x-1"],
        err=err, account="secondary",
    )
    msg = err.getvalue()
    assert "account skipped" in msg
    assert "secondary" in msg


def test_explicit_route_with_no_model_flag_still_suppresses_config_model():
    # An operator-typed --route with no -m must suppress the config model too,
    # not only a config-injected route: route_injected alone missed this case,
    # letting a config model land alongside an explicit --route (the exact
    # route+model collision this field exists to prevent).
    out = _inject(
        ["spawn", "--name", "w", "--route", "zai/glm-5.2[1m]", "/fno:target x-1"],
        model="opus",
    )
    assert "--model" not in out
    assert out[out.index("--route") + 1] == "zai/glm-5.2[1m]"


# --------------------------------------------------------------------------- #
# Autonomous lane reads the stage table (task 1.3)
#
# Autonomous dispatch (dispatch-node.sh) pins harness/substrate and passes the
# verb as the positional message. The profile keyed by that verb fills fields
# the dispatch has not itself pinned; an explicit flag still wins.
# --------------------------------------------------------------------------- #

def test_autonomous_dispatch_reads_blueprint_profile():
    # A /fno:blueprint spawn with a populated blueprint profile resolves that
    # coordinate (model here); the harness/substrate pins stand.
    out = _inject(
        ["spawn", "--harness", "claude", "--substrate", "bg", "--node", "x-1",
         "--name", "w", "/fno:blueprint x-1"],
        profiles={"blueprint": {"model": "fable"}},
    )
    assert out[out.index("--model") + 1] == "fable"


def test_autonomous_dispatch_without_profile_resolves_as_today():
    # Profile absent: nothing is injected beyond the explicit flags.
    out = _inject(
        ["spawn", "--harness", "claude", "--substrate", "bg", "--node", "x-1",
         "--name", "w", "/fno:blueprint x-1"],
    )
    assert "--model" not in out


def test_autonomous_dispatch_explicit_flag_beats_profile():
    # An explicit -m wins over the profile (the dispatch pinned the model).
    out = _inject(
        ["spawn", "-m", "haiku", "--harness", "claude", "--substrate", "bg",
         "--node", "x-1", "--name", "w", "/fno:blueprint x-1"],
        profiles={"blueprint": {"model": "fable"}},
    )
    assert out.count("--model") == 0  # only the explicit -m
    assert "fable" not in out


# --------------------------------------------------------------------------- #
# The receipt names the AXIS the field feeds, plus the route-collision
# refusal - a cross-axis collision (a profile-filled harness makes an
# already-typed route unusable), never a precedence bug.
# --------------------------------------------------------------------------- #

def test_ac1_hp_receipt_names_harness_axis_not_provider_field():
    # AC1-HP: `provider=agents.profiles.target` used to read as though a
    # provider was set to a profile. The real coordinate is the harness axis.
    err = io.StringIO()
    _inject(
        ["spawn", "--name", "w", "/fno:target x-1"], err=err,
        profiles={"target": _lane("codex", effort="high")},
    )
    msg = err.getvalue()
    assert "harness=codex (agents.profiles.target.provider)" in msg
    assert "effort=high (agents.profiles.target.effort)" in msg
    assert "provider=agents.profiles.target" not in msg


def test_ac2_hp_route_collision_refused_before_injection_dash_p_form():
    # The king's exact scenario: -P zai --model glm-5.3 under a profile that
    # fills a non-claude harness. Refuse BEFORE anything is injected.
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _inject(
            ["spawn", "--name", "t-x3ab0", "-P", "zai", "--model", "glm-5.3",
             "--substrate", "bg", "/fno:target x-1"],
            err=err, profiles={"target": _lane("codex")},
        )
    assert exc.value.code == 2
    msg = err.getvalue()
    assert "agents.profiles.target.provider = 'codex'" in msg
    assert "HARNESS axis" in msg
    assert "-P zai --model glm-5.3" in msg
    assert "-H claude" in msg
    assert "clear agents.profiles.target.provider" in msg
    assert "--harness" not in msg.split("\n")[0]  # nothing injected pre-refusal


def test_ac2_hp_route_collision_names_explicit_route_flag_not_dash_p():
    # AC5-HP twin: when the route came from --route, name --route, not -P.
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _inject(
            ["spawn", "--name", "w", "--route", "zai/glm-5.3", "/fno:target x-1"],
            err=err, profiles={"target": _lane("codex")},
        )
    assert exc.value.code == 2
    msg = err.getvalue()
    assert "--route zai/glm-5.3" in msg
    assert "-P zai" not in msg  # boilerplate still explains the -P axis; the caller's own flags do not name it


def test_ac4_edge_bare_vendor_no_model_is_not_route_shaped():
    # AC4-EDGE: -P with no --model is not yet a route; no collision refusal.
    out = _inject(
        ["spawn", "--name", "w", "-P", "zai", "/fno:target x-1"],
        profiles={"target": _lane("codex")},
    )
    assert "--harness" in out and out[out.index("--harness") + 1] == "codex"


def test_ac2_hp_route_shaped_but_profile_harness_is_claude_no_refusal():
    # The profile's harness CAN carry the route: no cross-axis collision.
    out = _inject(
        ["spawn", "--name", "w", "-P", "zai", "--model", "glm-5.3", "/fno:target x-1"],
        profiles={"target": _lane("claude")},
    )
    assert "--harness" in out and out[out.index("--harness") + 1] == "claude"


def test_ac3_hp_explicit_wins_every_injectable_field():
    # Per-field explicit-wins matrix: an explicit flag survives, the differing
    # profile value appears nowhere in the final argv, for every field.
    cases = [
        (["--harness", "codex"], _lane("claude"), "claude"),
        (["--model", "explicit-model"], {"model": "profile-model"}, "profile-model"),
        (["--harness", "claude", "--effort", "high"], {"effort": "low"}, "low"),
        (["--substrate", "pane"], {"substrate": "bg"}, "bg"),
        (["--permission-mode", "bypassPermissions"], {"permission_mode": "acceptEdits"}, "acceptEdits"),
        (["--route", "zai/glm-5.3"], {"route": "zai/other-model"}, "zai/other-model"),
        (["--harness", "claude", "--account", "primary"], {"account": "secondary"}, "secondary"),
    ]
    for explicit_flags, profile_fields, forbidden_value in cases:
        out = _inject(
            ["spawn", "--name", "w", *explicit_flags, "/fno:target x-1"],
            profiles={"target": profile_fields},
        )
        assert forbidden_value not in out, (explicit_flags, profile_fields)



# --- model-implies-vendor mismatch warning (change 5, spawn half) ------------


@requires_rust
def test_model_vendor_mismatch_warns_naming_both_sides():
    # --model glm-5.3 with no zai route resolved: the spawn proceeds AND warns,
    # naming the implied vendor (zai) and the resolved lane (anthropic, the
    # builtin default harness). Warn-only: nothing is refused or rewritten.
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "-m", "glm-5.3", "hi"], err=err, model="opus"
    )
    assert "glm-5.3" in out  # the model still rides the argv; nothing is refused
    msg = err.getvalue()
    assert "glm-5.3" in msg and "zai" in msg and "anthropic" in msg


def test_model_vendor_match_prints_no_warning():
    # The negative is load-bearing: a matching model must print nothing, or the
    # warning becomes noise and gets ignored - how tonight's misroute survived.
    err = io.StringIO()
    _inject(["spawn", "--name", "w", "-m", "opus", "hi"], err=err, model="opus")
    assert "implies vendor" not in err.getvalue()


def test_route_matching_model_is_silent():
    # --route owns the vendor; a model half matching it is the intended shape.
    err = io.StringIO()
    _inject(
        ["spawn", "--name", "w", "--route", "zai/glm-5.3", "hi"], err=err, model="opus"
    )
    assert "implies vendor" not in err.getvalue()


@requires_rust
def test_mismatch_warns_with_no_config_at_all():
    # The warning must not depend on config being present: a bare argv with a
    # cross-vendor model is the exact operator typo it exists to catch.
    err = io.StringIO()
    inject_spawn_defaults(
        ["spawn", "--name", "w", "-m", "gpt-5.6", "hi"], stderr=err, env={}
    )
    assert "openai" in err.getvalue() and "anthropic" in err.getvalue()


def test_explicit_route_with_cross_vendor_model_is_silent():
    # --route zai,glm-5.2 with an explicit --model opus is the documented
    # deliberate override (the model beats the route's model). No warning:
    # both halves were named by the caller, not misrouted by a default.
    err = io.StringIO()
    _inject(
        ["spawn", "--name", "w", "-H", "claude", "--route", "zai,glm-5.2",
         "-m", "opus", "hi"],
        err=err,
        model="opus",
    )
    assert "implies vendor" not in err.getvalue()


@requires_rust
def test_injected_cross_vendor_model_refuses_and_names_the_config_key():
    # The specimen, 2026-08-21: agents.defaults.model was a gpt-* id, every
    # spawn that named no model inherited it, and the worker started, reported
    # live, and died on its first inference. Nobody typed that pairing, so the
    # spawn refuses instead of warning. The message must name the config key,
    # because a caller who typed nothing has no other half to edit.
    err = io.StringIO()
    with pytest.raises(SystemExit) as excinfo:
        _inject(["spawn", "--name", "w", "hi"], err=err, model="gpt-5.6")
    assert excinfo.value.code == 2
    msg = err.getvalue()
    assert "agents.defaults.model" in msg
    assert "openai" in msg and "anthropic" in msg


@requires_rust
def test_typed_cross_vendor_model_still_warns_and_proceeds():
    # The other half of the same predicate, and the one that must NOT change.
    # A caller who types a cross-vendor model means it; passthrough is
    # deliberate and documented. Config also names a model here, so this pins
    # that the refusal keys on what was INJECTED, not on config being present.
    err = io.StringIO()
    out = _inject(["spawn", "--name", "w", "-m", "gpt-5.6", "hi"], err=err, model="opus")
    assert "gpt-5.6" in out
    msg = err.getvalue()
    assert "implies vendor" in msg
    assert "refusing to spawn" not in msg


@requires_rust
def test_injected_model_matching_the_lane_is_silent():
    # The negative on the refusal path: an injected model whose vendor MATCHES
    # the lane is the ordinary case and must neither warn nor refuse. Without
    # this, a refusal that fired on every injected model would pass the test
    # above while stopping the fleet spawning.
    err = io.StringIO()
    out = _inject(["spawn", "--name", "w", "hi"], err=err, model="opus")
    assert "opus" in out
    assert err.getvalue() == "" or "implies vendor" not in err.getvalue()


@requires_rust
def test_account_in_play_downgrades_the_refusal_to_a_warning():
    """An account can carry its own vendor credential, and `resolve_lane_vendor`
    never reads the `--account` axis.

    So a zai account beside a glm model on a claude harness is a spawn that
    WORKS, which this check sees as a mismatch. Refusing it would stop the
    fleet spawning to prevent a failure that was not going to happen, and the
    `-P` escape the refusal suggests composes a different credential and bill.
    """
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "hi"], err=err, model="glm-5.2", account="zai-main"
    )
    assert "refusing to spawn" not in err.getvalue()
    assert "implies vendor" in err.getvalue()
    assert out[0] == "spawn"


def test_injected_cross_vendor_model_with_explicit_route_proceeds():
    # --route names the lane, so the caller chose both halves even though the
    # model arrived by injection. Refusing here would break a legal override.
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "--route", "openai/gpt-5.6", "hi"],
        err=err,
        model="gpt-5.6",
    )
    assert "refusing to spawn" not in err.getvalue()
    assert out[0] == "spawn"


@requires_rust
def test_lane_vendor_resolves_unrouted_harness_from_final_argv():
    assert resolve_lane_vendor(["codex", "-C", "/tmp/workspace"]) == "openai"


@requires_rust
def test_model_vendor_mismatch_emits_measurement_event(monkeypatch):
    emitted = []
    monkeypatch.setattr(
        "fno.agents.events.emit",
        lambda kind, **data: emitted.append((kind, data)),
    )
    err = io.StringIO()
    inject_spawn_defaults(
        ["spawn", "--name", "w", "-H", "codex", "-m", "opus", "hi"],
        stderr=err,
        env={},
    )
    # `model_source` and `outcome` ride the event because the measurement is
    # useless without them: a warned typed pairing and a refused injected one
    # are different facts, and the old payload rendered them identically.
    # The seam's own spawn_defaults_applied decision event (task 0.1) rides
    # the same emit; it carries no mismatch fields, so filter on kinds.
    mismatch = [e for e in emitted if e[0] == "model_vendor_mismatch"]
    assert mismatch == [
        (
            "model_vendor_mismatch",
            {
                "model": "opus",
                "implied_vendor": "anthropic",
                "resolved_vendor": "openai",
                "model_source": "explicit",
                "outcome": "warned",
            },
        )
    ]
    kinds = [k for k, _ in emitted]
    assert kinds.count("spawn_defaults_applied") == 1


@requires_rust
def test_refused_mismatch_event_names_the_config_key_that_supplied_the_model(
    monkeypatch,
):
    """The refusal is measurable as a refusal, not just as a mismatch.

    Reading these events later, the question is which spawns were STOPPED and
    which config key stopped them. An `outcome` that always read the same word
    could not answer it.
    """
    emitted = []
    monkeypatch.setattr(
        "fno.agents.events.emit",
        lambda kind, **data: emitted.append((kind, data)),
    )
    with pytest.raises(SystemExit):
        _inject(["spawn", "--name", "w", "hi"], err=io.StringIO(), model="gpt-5.6")
    assert emitted == [
        (
            "model_vendor_mismatch",
            {
                "model": "gpt-5.6",
                "implied_vendor": "openai",
                "resolved_vendor": "anthropic",
                "model_source": "agents.defaults.model",
                "outcome": "refused",
            },
        )
    ]


@requires_rust
def test_capped_lane_does_not_refuse_a_spawn_that_names_its_own_lane(monkeypatch):
    """A cap names a VENDOR's concurrency. A caller who typed --harness codex is
    not spending the capped zai lane's budget, so refusing that spawn stops work
    the cap was never about."""
    import fno.agents.spawn_defaults as spawn_defaults
    import fno.agents.spawn_gate as spawn_gate

    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [])
    monkeypatch.setattr(spawn_gate, "provider_live_count", lambda vendor: 2)
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "--harness", "codex", "/fno:target x-1"],
        err=err,
        max_lanes={"zai": 2},
        profiles={"target": {"lanes": [
            _lane("claude", route="zai/glm-5.3[1m]", substrate="bg"),
        ]}},
    )
    assert out[out.index("--harness") + 1] == "codex"
    assert "already names the lane" in err.getvalue()
    # The lane's other fields must not ride in either: no lane was applied.
    assert "--substrate" not in out or out[out.index("--substrate") + 1] != "bg"


def test_gate_bypass_disables_the_cap_refusal_but_not_the_skip(monkeypatch):
    """FNO_SPAWN_GATE=0 is the admission escape and its contract is that it never
    blocks a spawn. Cap-SKIPPING still runs: steering onto a free lane blocks
    nothing, and dropping it would send every bypassed spawn at a saturated
    vendor."""
    import fno.agents.spawn_defaults as spawn_defaults
    import fno.agents.spawn_gate as spawn_gate

    monkeypatch.setenv("FNO_SPAWN_GATE", "0")
    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [])
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    monkeypatch.setattr(spawn_gate, "provider_live_count", lambda vendor: 2)
    err = io.StringIO()

    # Two lanes, one capped: the free lane is still chosen rather than refused.
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"],
        err=err,
        max_lanes={"zai": 2},
        profiles={"target": {"lanes": [
            _lane("claude", route="zai/glm-5.3[1m]"),
            _lane("codex"),
        ]}},
    )
    assert out[out.index("--harness") + 1] == "codex"
    assert "provider zai at 2 of 2" in err.getvalue()

    # Only lane capped: no refusal under the bypass.
    err2 = io.StringIO()
    _inject(
        ["spawn", "--name", "w", "/fno:target x-1"],
        err=err2,
        max_lanes={"zai": 2},
        profiles={"target": {"lanes": [
            _lane("claude", route="zai/glm-5.3[1m]"),
        ]}},
    )
    assert "FNO_SPAWN_GATE=0" in err2.getvalue()


class _Routing:
    def __init__(self, models):
        self.models = models


def _slot_settings(rows, profiles):
    """Settings whose DECLARED routing inventory is exactly ``rows``.

    String lanes resolve against ``settings.routing.models`` - the declared
    rows - never the built-in fallback, so the fake must carry the rows the
    lanes name.
    """
    s = _Settings(profiles=profiles)
    s.routing = _Routing(rows)
    return s


_SLOT_ROWS = [
    {"name": "flash-x", "harness": "claude", "model": "glm-5.3-flash",
     "band": "low", "account": "zai-main"},
    {"name": "sonnet-x", "harness": "claude", "model": "claude-sonnet-5",
     "band": "medium"},
]


@requires_rust
def test_string_lane_names_an_inventory_row(monkeypatch):
    """A lane may be the NAME of a [[routing.models]] row: the row's harness,
    model and access path ride as one coordinate."""
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(_SLOT_ROWS, {"target": {"lanes": ["flash-x"]}}),
        stderr=err,
        env={},
    )
    assert out[out.index("--harness") + 1] == "claude"
    assert out[out.index("--model") + 1] == "glm-5.3-flash"
    assert "applied slot=agents.profiles.target.lanes[0] flash-x (routing)" in err.getvalue()


@requires_rust
def test_lane_on_exhausted_account_is_skipped_for_the_next_lane(monkeypatch):
    """AC3-HP: the lane whose account is dead skips; the sibling lane on the
    healthy account answers."""
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": {"state": "ok", "accounts": {"zai-main": "exhausted"}}},
    )
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(
            _SLOT_ROWS, {"target": {"lanes": ["flash-x", "sonnet-x"]}}
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--model") + 1] == "claude-sonnet-5"
    assert "capacity=exhausted" in err.getvalue()


def test_on_exhausted_queue_exits_78_with_typed_refusal(monkeypatch, capsys):
    """AC3-EDGE: every lane exhausted + on_exhausted=queue exits 78 with the
    typed capacity refusal - the shape a dispatcher reads as capacity, not
    config."""
    # The hermetic suite sets FNO_SPAWN_GATE=0, and that escape degrades
    # instead of refusing; opt back in or this asserts nothing.
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    both_dead = [
        dict(_SLOT_ROWS[0]),
        dict(_SLOT_ROWS[1], account="claude-main"),
    ]
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {
            "claude": {
                "state": "exhausted",
                "accounts": {"zai-main": "exhausted", "claude-main": "exhausted"},
            }
        },
    )
    with pytest.raises(SystemExit) as exc:
        inject_spawn_defaults(
            ["spawn", "--name", "w", "/fno:target x-1"],
            settings=_slot_settings(
                both_dead,
                {"target": {"lanes": ["flash-x", "sonnet-x"], "on_exhausted": "queue"}},
            ),
            stderr=io.StringIO(),
            env={},
        )
    assert exc.value.code == 78
    receipt = json.loads(capsys.readouterr().out)
    assert receipt["status"] == "refused"
    assert receipt["reason"] == "slot_exhausted"
    assert receipt["verb"] == "target"
    assert [lane["name"] for lane in receipt["lanes"]] == ["flash-x", "sonnet-x"]
    assert all("exhausted" in lane["reason"] for lane in receipt["lanes"])


def test_on_exhausted_degrade_names_the_degrade_in_the_receipt(monkeypatch):
    """on_exhausted=degrade: the profile scalars answer as before, and the
    receipt says the slot terminal was the reason."""
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": {"state": "ok", "accounts": {"zai-main": "exhausted"}}},
    )
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(
            _SLOT_ROWS,
            {
                "target": {
                    "lanes": ["flash-x"],
                    "on_exhausted": "degrade",
                    "model": "fallback-m",
                }
            },
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--model") + 1] == "fallback-m"
    assert "applied slot=exhausted degrade" in err.getvalue()


@requires_rust
def test_unknown_lane_name_refuses_by_name(monkeypatch):
    """AC3-ERR: a lane naming no declared row refuses with exit 2, naming the
    lane path, the missing row, and the declared row names."""
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        inject_spawn_defaults(
            ["spawn", "--name", "w", "/fno:target x-1"],
            settings=_slot_settings(
                _SLOT_ROWS, {"target": {"lanes": ["ghost-x"]}}
            ),
            stderr=err,
            env={},
        )
    assert exc.value.code == 2
    msg = err.getvalue()
    assert "agents.profiles.target.lanes[0]" in msg
    assert "'ghost-x'" in msg
    assert "flash-x" in msg and "sonnet-x" in msg
    assert "fno config route inventory" in msg


@requires_rust
def test_inline_lane_still_selects(monkeypatch):
    """The inline-table lane spelling keeps working after the port: sugar over
    the same resolver, never a second leg."""
    import fno.agents.spawn_defaults as spawn_defaults

    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [])
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"],
        err=err,
        profiles={"target": {"lanes": [_lane("codex", effort="high")]}},
    )
    assert out[out.index("--harness") + 1] == "codex"
    assert "agents.profiles.target.lanes[0]" in err.getvalue()


@requires_rust
def test_verb_with_no_lanes_falls_to_the_grid(monkeypatch):
    """A profile without lanes changes nothing: the capacity grid over the
    whole inventory answers, exactly as before the slot resolver existed."""
    _declare_inventory(monkeypatch, _two_harness_rows())
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda *args, **kwargs: {"difficulty": "high", "priority": "p1"},
    )
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": "exhausted", "codex": "ok"},
    )
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "--node", "x-grid2", "hi"],
        err=err,
        profiles={"target": {"substrate": "bg"}},
    )
    assert out[out.index("--harness") + 1] == "codex"
    # the chain's terminal is the grid's own pick line; the seam receipts it
    assert "applied grid=grid candidate codex/sol-x capacity=ok" in err.getvalue()


@requires_rust
def test_lane_validation_refusals_run_on_real_dict_lanes(monkeypatch):
    """Live config lanes arrive as raw TOML dicts, not objects. Every other lane
    test builds objects, which take the getattr branch, so the Mapping-only
    unknown-field and non-string refusals were never executed."""
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})

    def _raw_lane_settings(lanes):
        prof = type("P", (), {"lanes": lanes})()
        return type(
            "S",
            (),
            {"agents": type(
                "A", (), {"defaults": _Defaults(), "profiles": {"target": prof},
                          "max_lanes": {}}
            )},
        )()

    for lanes, fragment in (
        ([{"provider": "claude", "nonsense": "x"}], "unknown field 'nonsense'"),
        ([{"provider": 7}], "must be a string"),
        ([{}], "is empty"),
    ):
        err = io.StringIO()
        with pytest.raises(SystemExit) as exc:
            inject_spawn_defaults(
                ["spawn", "--name", "w", "/fno:target x-1"],
                settings=_raw_lane_settings(lanes),
                stderr=err,
                env={},
            )
        assert exc.value.code == 2, fragment
        assert fragment in err.getvalue()
        assert "no worker launched" in err.getvalue()


@requires_rust
def test_config_pane_group_degrades_open_beside_an_explicit_split(monkeypatch):
    """dispatch hard-refuses a pane group beside --split/--at. That refusal is
    right for a group the operator TYPED and wrong for one config injected: it
    would fail-close a spawn on a value the caller never asked for."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "--split", "right", "/fno:target x-1"],
        err=err,
        profiles={"target": _lane("codex", substrate="pane", pane_group="codex")},
    )
    assert "--tab" not in out
    assert "pane group skipped" in err.getvalue()
    assert "--split" in err.getvalue()
    # The rest of the lane still applies; only the group was dropped.
    assert out[out.index("--harness") + 1] == "codex"


@requires_rust
def test_config_pane_group_still_injects_without_a_conflicting_flag(monkeypatch):
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"],
        err=err,
        profiles={"target": _lane("codex", substrate="pane", pane_group="codex")},
    )
    assert out[out.index("--tab") + 1] == "codex"


@requires_rust
def test_config_pane_group_degrades_open_beside_once(monkeypatch):
    """cli.py refuses placement on `substrate != "pane" OR once`, so a one-shot
    spawn has no pane geometry even though its substrate resolves to pane. The
    injected group must skip there too, or it fail-closes a spawn on a value the
    caller never typed."""
    err = io.StringIO()
    # --once alone resolves the substrate to headless, which the substrate
    # branch already catches. The gap is an EXPLICIT --substrate pane beside it:
    # eff_substrate is then "pane" and only the --once scan can skip the group.
    out = _inject(
        ["spawn", "--name", "w", "--substrate", "pane", "--once", "/fno:target x-1"],
        err=err,
        profiles={"target": _lane("codex", substrate="pane", pane_group="codex")},
    )
    assert "--tab" not in out
    assert "pane group skipped" in err.getvalue()
    assert "--once" in err.getvalue()


@requires_rust
def test_config_pane_group_survives_a_fenced_provider_argv(monkeypatch):
    """`spawn ... -- claude --at 3` names a SEED token, not an fno flag. Scanning
    raw argv would drop the config's pane_group and blame a flag the caller never
    passed to fno."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1", "--", "claude", "--at", "3"],
        err=err,
        profiles={"target": _lane("codex", substrate="pane", pane_group="codex")},
    )
    assert out[out.index("--tab") + 1] == "codex"
    assert "pane group skipped" not in err.getvalue()


def test_config_pane_group_defers_to_a_valueless_trailing_tab(monkeypatch):
    """A value read answers None for a trailing bare `--tab`, so injecting beside
    it puts TWO --tab tokens in the argv and click fails the spawn on the
    operator's own flag."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1", "--tab"],
        err=err,
        profiles={"target": _lane("codex", substrate="pane", pane_group="codex")},
    )
    assert out.count("--tab") == 1


@requires_rust
def test_config_pane_group_skips_on_a_glued_short_placement_flag(monkeypatch):
    """click accepts `-xdown`. Missing that spelling let a real placement flag
    read as absent, inject the group, and then hit the hard refusal on a value
    the operator never typed."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "-xdown", "/fno:target x-1"],
        err=err,
        profiles={"target": _lane("codex", substrate="pane", pane_group="codex")},
    )
    assert "--tab" not in out
    assert "pane group skipped" in err.getvalue()
    assert "-x" in err.getvalue()


@requires_rust
def test_capped_lane_escape_also_honours_the_vendor_flag(monkeypatch):
    """A cap names a VENDOR, and -P names the vendor, so a caller who typed it is
    not spending a capped lane's budget. Both this function's docstring and the
    shipped routing doc promise -P alongside --harness."""
    import fno.agents.spawn_defaults as spawn_defaults
    import fno.agents.spawn_gate as spawn_gate

    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [])
    monkeypatch.setattr(spawn_gate, "provider_live_count", lambda vendor: 2)
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "-P", "zai", "/fno:target x-1"],
        err=err,
        max_lanes={"zai": 2},
        profiles={"target": {"lanes": [
            _lane("claude", route="zai/glm-5.3[1m]"),
        ]}},
    )
    assert "already names the lane" in err.getvalue()
    assert out  # the spawn continues rather than exiting 2


@requires_rust
def test_a_selected_lane_does_not_inherit_a_route_it_never_named(monkeypatch):
    """A lane is a COMPLETE routing coordinate. Per-field fallback let a codex
    lane inherit the profile's zai route, producing `--harness codex --route
    zai/...` in one argv, which cli.py refuses outright."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"],
        err=err,
        profiles={"target": {
            "route": "zai/glm-5.3[1m]",
            "lanes": [_lane("codex")],
        }},
    )
    assert out[out.index("--harness") + 1] == "codex"
    assert "--route" not in out
_LOW_NODE = {"id": "x-1", "difficulty": "low", "priority": "p2"}


def test_missing_difficulty_takes_the_high_overlay(monkeypatch):
    """AC6-DIFFICULTY: no node, no difficulty: the high overlay answers and
    the receipt says the difficulty rounded up."""
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(
            _SLOT_ROWS,
            {"target": {"lanes": ["flash-x"],
                        "by_difficulty": {"high": {"lanes": ["sonnet-x"]}}}},
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--model") + 1] == "claude-sonnet-5"
    assert "difficulty missing; rounds up to high" in err.getvalue()


@requires_rust
def test_low_difficulty_overlay_replaces_lanes(monkeypatch):
    """AC6-DIFFICULTY: a low node rides the low overlay's lanes."""
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node", lambda toks, env=None: dict(_LOW_NODE)
    )
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "--node", "x-1", "/fno:target x-1"],
        settings=_slot_settings(
            _SLOT_ROWS,
            {"target": {"lanes": ["sonnet-x"],
                        "by_difficulty": {"low": {"lanes": ["flash-x"]}}}},
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--model") + 1] == "glm-5.3-flash"


def test_invalid_difficulty_rounds_up_to_high(monkeypatch):
    """AC6-DIFFICULTY: an out-of-vocabulary difficulty is missing, not low."""
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node",
        lambda toks, env=None: {"id": "x-1", "difficulty": "urgent"},
    )
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "--node", "x-1", "/fno:target x-1"],
        settings=_slot_settings(
            _SLOT_ROWS,
            {"target": {"lanes": ["flash-x"],
                        "by_difficulty": {"high": {"lanes": ["sonnet-x"]}}}},
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--model") + 1] == "claude-sonnet-5"
    assert "difficulty 'urgent' is not low|medium|high" in err.getvalue()


@requires_rust
def test_overlay_omitted_fields_inherit_the_base_slot(monkeypatch):
    """AC6-DIFFICULTY: an overlay that only names a policy keeps the base
    lanes; the policy is live on them."""
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": {"state": "ok", "accounts": {"zai-main": "low"}}},
    )
    monkeypatch.setattr(
        "fno.agents.spawn_defaults._grid_node", lambda toks, env=None: dict(_LOW_NODE)
    )
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "--node", "x-1", "/fno:target x-1"],
        settings=_slot_settings(
            _SLOT_ROWS,
            {"target": {"lanes": ["flash-x", "sonnet-x"],
                        "by_difficulty": {"low": {"on_low": "skip"}}}},
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--model") + 1] == "claude-sonnet-5"
    assert "capacity=low (on_low=skip)" in err.getvalue()


_LOW_FLASH_HEALTHY_CODEX = [
    *_SLOT_ROWS[:1],
    {"name": "codex-y", "harness": "codex", "model": "gpt-5.6-luna"},
]


@requires_rust
def test_on_low_prefer_healthy_demotes_low_behind_healthy(monkeypatch):
    """AC6-LOW: the default policy demotes a low lane behind a healthy one."""
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {
            "claude": {"state": "low", "accounts": {"zai-main": "low"}},
            "codex": {"state": "ok"},
        },
    )
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(
            _LOW_FLASH_HEALTHY_CODEX,
            {"target": {"lanes": ["flash-x", "codex-y"]}},
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--harness") + 1] == "codex"
    assert "slot demote agents.profiles.target.lanes[0] flash-x capacity=low" in err.getvalue()


@requires_rust
def test_on_low_prefer_healthy_takes_the_demoted_lane_when_all_low(monkeypatch):
    """AC6-LOW: no healthy lane anywhere: the first low lane still serves."""
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {
            "claude": {"state": "low", "accounts": {"zai-main": "low"}},
            "codex": {"state": "low"},
        },
    )
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(
            _LOW_FLASH_HEALTHY_CODEX,
            {"target": {"lanes": ["flash-x", "codex-y"]}},
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--harness") + 1] == "claude"
    assert "slot demote agents.profiles.target.lanes[0] flash-x capacity=low" in err.getvalue()
    assert "applied slot=agents.profiles.target.lanes[0] flash-x (routing)" in err.getvalue()


@requires_rust
def test_on_unknown_skip_excludes_unknown_lanes_and_refuses(monkeypatch):
    """AC6-UNKNOWN: with skip, an unproven observation never serves."""
    monkeypatch.setenv("FNO_SPAWN_GATE", "1")
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        inject_spawn_defaults(
            ["spawn", "--name", "w", "/fno:target x-1"],
            settings=_slot_settings(
                _SLOT_ROWS,
                {"target": {"lanes": ["flash-x", "sonnet-x"], "on_unknown": "skip"}},
            ),
            stderr=err,
            env={},
        )
    assert exc.value.code == 2
    assert "capacity=unknown (on_unknown=skip)" in err.getvalue()


@requires_rust
def test_overlay_with_explicit_empty_lanes_refuses_as_malformed(monkeypatch):
    """AC6-DIFFICULTY: an explicitly empty overlay lane list is malformed, not
    an invitation to open the global inventory."""
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        inject_spawn_defaults(
            ["spawn", "--name", "w", "/fno:target x-1"],
            settings=_slot_settings(
                _SLOT_ROWS,
                {"target": {"lanes": ["flash-x"],
                            "by_difficulty": {"high": {"lanes": []}}}},
            ),
            stderr=err,
            env={},
        )
    assert exc.value.code == 2
    assert "by_difficulty.high.lanes must be a non-empty list" in err.getvalue()


def test_overlay_only_profile_still_resolves(monkeypatch):
    """AC6-DIFFICULTY: a profile with no base lanes but a by_difficulty map is
    a configured slot, not a grid fallthrough."""
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(
            _SLOT_ROWS,
            {"target": {"by_difficulty": {"high": {"lanes": ["sonnet-x"]}}}},
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--model") + 1] == "claude-sonnet-5"
_IDENTITY_ROWS = [
    {"name": "canon-opus", "harness": "claude", "model": "opus",
     "account": "makers"},
    {"name": "alt-sonnet", "harness": "claude", "model": "sonnet",
     "account": "readyrule"},
]


@requires_rust
def test_identity_mismatch_pin_is_always_excluded(monkeypatch):
    """AC6-PIN: the slot proves makers is active; a readyrule pin is a
    mismatch and never serves, whatever on_unknown allows."""
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {
            "claude": {
                "state": "ok",
                "accounts": {"makers": "ok", "readyrule": "ok"},
                "evidence": {"makers": "proven", "readyrule": "mismatch"},
            },
        },
    )
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(
            _IDENTITY_ROWS,
            {"target": {"lanes": ["alt-sonnet", "canon-opus"],
                        "on_unknown": "allow"}},
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--model") + 1] == "opus"
    assert "account_identity_mismatch" in err.getvalue()


@requires_rust
def test_identity_unknown_is_governed_by_on_unknown(monkeypatch):
    """AC6-IDENTITY: an unproven slot claim is excluded under skip and named
    under the default allow."""
    capacity = {
        "claude": {"state": "unknown", "accounts": {"makers": "ok"},
                   "evidence": {}},
    }
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(
            _IDENTITY_ROWS[:1], {"target": {"lanes": ["canon-opus"]}}
        ),
        stderr=err,
        env={},
    )
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: capacity)
    assert out[out.index("--harness") + 1] == "claude"

    monkeypatch.setenv("FNO_SPAWN_GATE", "1")
    err2 = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        inject_spawn_defaults(
            ["spawn", "--name", "w", "/fno:target x-1"],
            settings=_slot_settings(
                _IDENTITY_ROWS[:1],
                {"target": {"lanes": ["canon-opus"], "on_unknown": "skip"}},
            ),
            stderr=err2,
            env={},
        )
    assert exc.value.code == 2
    assert "account_identity_unknown (on_unknown=skip)" in err2.getvalue()


@requires_rust
def test_vendor_route_lane_never_claims_the_slot(monkeypatch):
    """AC6-IDENTITY: an API lane with its own account and route skips the
    identity gate; the slot occupant is not its business."""
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {
            "claude": {
                "state": "ok",
                "accounts": {"zai-main": "ok"},
                "evidence": {},
            },
        },
    )
    rows = [{"name": "flash-zai", "harness": "claude", "model": "glm",
             "route": "zai/glm-5.3", "account": "zai-main"}]
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(rows, {"target": {"lanes": ["flash-zai"]}}),
        stderr=err,
        env={},
    )
    assert out[out.index("--harness") + 1] == "claude"
    assert "account_identity" not in err.getvalue()


def test_proven_account_owns_the_harness_aggregate(monkeypatch):
    """AC6-IDENTITY: an unpinned row reads the proven account's state, never
    a MAX that a sibling record could fake."""
    from fno.route_resolve import runtime_capacity as rc

    monkeypatch.setattr(
        "fno.route_resolve.harness_accounts", lambda harness, **kw: ["makers", "readyrule"]
    )

    class _V:
        def __init__(self, state):
            self.state = type("S", (), {"value": state})()
            self.resets_at = None
            self.source = "window"

    monkeypatch.setattr(
        "fno.adapters.providers.runtime_state.headrooms",
        lambda ids: {"makers": _V("exhausted"), "readyrule": _V("ok")},
    )
    monkeypatch.setattr(
        "fno.route_resolve._identity_evidence",
        lambda harness, accounts: {"makers": "proven", "readyrule": "mismatch"},
    )
    cap = rc(providers=("claude",))
    assert cap["claude"]["state"] == "exhausted"
    assert cap["claude"]["window"] == "identity:makers"
@requires_rust
def test_lane_coordinate_forwards_route_and_account(monkeypatch):
    """AC6-COORDINATE: a named row's vendor route and account constraint ride
    the launch argv; the coordinate is not discarded after the capacity check."""
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {"claude": {"state": "ok", "accounts": {"zai-main": "ok"},
                                 "evidence": {}}},
    )
    rows = [{"name": "flash-zai", "harness": "claude", "model": "glm",
             "route": "zai/glm-5.3", "account": "zai-main"}]
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_slot_settings(rows, {"target": {"lanes": ["flash-zai"]}}),
        stderr=err,
        env={},
    )
    assert out[out.index("--route") + 1] == "zai/glm-5.3"
    assert out[out.index("--account") + 1] == "zai-main"


def test_record_route_contradiction_refuses(monkeypatch):
    """AC6-COORDINATE: the account record resolves its own vendor; a lane
    route that contradicts it would check one coordinate and bill another."""
    from types import SimpleNamespace

    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    rows = [{"name": "flash-zai", "harness": "claude", "model": "glm",
             "route": "zai/glm-5.3", "account": "zai-main"}]
    s = _slot_settings(rows, {"target": {"lanes": ["flash-zai"]}})
    s.accounts = SimpleNamespace(records=[{"id": "zai-main", "route": "openai/x"}])
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        inject_spawn_defaults(
            ["spawn", "--name", "w", "/fno:target x-1"],
            settings=s,
            stderr=err,
            env={},
        )
    assert exc.value.code == 2
    assert "contradicting the lane route 'zai/glm-5.3'" in err.getvalue()


@requires_rust
def test_explicit_model_pin_overrides_the_lanes(monkeypatch):
    """AC6-COORDINATE: a typed --model outranks the slot, receipt names the
    override, and no lane harness is borrowed for the foreign model."""
    monkeypatch.setattr("fno.route_resolve.runtime_capacity", lambda **kw: {})
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "--model", "gpt-5.6-luna", "/fno:target x-1"],
        settings=_slot_settings(
            _SLOT_ROWS, {"target": {"lanes": ["flash-x", "sonnet-x"]}}
        ),
        stderr=err,
        env={},
    )
    assert out[out.index("--model") + 1] == "gpt-5.6-luna"
    assert "slot=model-pin-override" in err.getvalue()
    applied = err.getvalue()
    assert "applied slot=" not in applied or "model-pin-override" in applied


# ---------------------------------------------------------------------------
# Harness-keyed spawn defaults (x-8975): the overlay rungs
# ---------------------------------------------------------------------------


_PROFILE_OVERLAY = {
    "target": {
        "permission_mode": "yolo",
        "effort": "high",
        "harness": {
            "claude": {"permission_mode": "bypassPermissions"},
            "codex": {"effort": "xhigh"},
        },
    },
}


@requires_rust
def test_profile_harness_overlay_answers_claude_scalar_answers_codex():
    """AC2-HP: the same verb carries two harnesses' answers to one question.

    -H claude reads profiles.target.harness.claude; -H codex falls through to
    the profiles.target scalar, which is a codex spelling."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "-H", "claude", "--name", "w", "/target x"],
        err=err,
        profiles=_PROFILE_OVERLAY,
    )
    assert out[out.index("--permission-mode") + 1] == "bypassPermissions"
    assert "agents.profiles.target.harness.claude.permission_mode" in err.getvalue()

    err = io.StringIO()
    out = _inject(
        ["spawn", "-H", "codex", "--name", "w", "/target x"],
        err=err,
        profiles=_PROFILE_OVERLAY,
    )
    assert out[out.index("--permission-mode") + 1] == "yolo"
    assert "agents.profiles.target.permission_mode" in err.getvalue()


@requires_rust
def test_effort_overlay_read_happens_after_harness_resolution():
    """AC2-EDGE: no -H on the argv; the profile's own provider=codex resolves
    the harness, and the effort read through THAT harness picks xhigh."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/target x"],
        err=err,
        profiles={
            "target": {
                "provider": "codex",
                "effort": "high",
                "harness": {"codex": {"effort": "xhigh"}},
            },
        },
    )
    assert out[out.index("--harness") + 1] == "codex"
    assert out[out.index("--effort") + 1] == "xhigh"
    assert "agents.profiles.target.harness.codex.effort" in err.getvalue()


@requires_rust
def test_defaults_harness_overlay_answers_when_profile_is_silent():
    """The defaults rung keeps its own harness table: a codex answer there
    wins on -H codex over the defaults scalar, with no profile in play."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "-H", "codex", "--name", "w", "hi"],
        err=err,
        permission_mode="bypassPermissions",
        harness={"codex": {"permission_mode": "yolo"}},
    )
    assert out[out.index("--permission-mode") + 1] == "yolo"
    assert "agents.defaults.harness.codex.permission_mode" in err.getvalue()


def test_explicit_flag_still_beats_every_overlay_rung():
    """Precedence head: an explicit --permission-mode wins over lane, overlay
    and scalar alike."""
    out = _inject(
        ["spawn", "-H", "claude", "--permission-mode", "plan", "--name", "w", "/target x"],
        profiles=_PROFILE_OVERLAY,
    )
    assert out[out.index("--permission-mode") + 1] == "plan"
    assert out.count("--permission-mode") == 1


@requires_rust
def test_harness_args_appended_behind_tail_fence():
    """The overlay bundle rides the -- passthrough fence at the argv TAIL, so
    the caller's own pre-fence tokens stay pre-fence."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "-H", "codex", "--name", "w", "hi"],
        err=err,
        harness={"codex": {"args": ["--profile", "fno"]}},
    )
    i = out.index("--")
    assert out[i + 1 : i + 3] == ["--profile", "fno"]
    assert "agents.defaults.harness.codex.args" in err.getvalue()
    assert "unverified" in err.getvalue()


@requires_rust
def test_harness_args_skipped_when_argv_already_fenced():
    """AC2-ERR: the caller's fence selects their complete bundle; the
    configured one is displaced by name, and no second fence is added."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "-H", "codex", "--name", "w", "hi", "--", "--profile", "other"],
        err=err,
        harness={"codex": {"args": ["--profile", "fno"]}},
    )
    assert out.count("--") == 1
    assert "harness args skipped" in err.getvalue()
    assert "agents.defaults.harness.codex.args" in err.getvalue()


@requires_rust
def test_harness_args_skipped_behind_an_argv_payload():
    """The --argv payload boundary owns everything after it too (the Rust
    parser reads it as the provider command line), so it displaces the
    configured bundle the same way a typed fence does."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "-H", "claude", "--argv", "--", "claude", "--at", "3"],
        err=err,
        harness={"claude": {"args": ["--settings", "a.json"]}},
    )
    assert "--settings" not in out
    assert "harness args skipped" in err.getvalue()
    assert "--argv" in err.getvalue()


@requires_rust
def test_bundle_reserves_the_empty_message_slot():
    """A pane spawn with no prompt keeps its message slot: click fills
    positionals in order, so without the explicit empty the bundle's first
    token becomes the worker seed."""
    out = _inject(
        ["spawn", "-H", "codex", "--name", "w"],
        harness={"codex": {"args": ["--profile", "fno"]}},
    )
    assert out[-4:] == ["", "--", "--profile", "fno"]


@requires_rust
def test_lane_args_win_over_overlay_bundle():
    """The lane rung sits above the overlays for args too, and bundles are
    never concatenated."""
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/target x"],
        err=err,
        profiles={
            "target": {
                "lanes": [_lane("codex", effort="high", args=["--profile", "lane"])],
                "harness": {"codex": {"args": ["--profile", "overlay"]}},
            },
        },
    )
    i = out.index("--")
    assert out[i + 1 : i + 3] == ["--profile", "lane"]
    assert "overlay" not in out
    assert ".lanes[0].args" in err.getvalue()


@requires_rust
def test_unknown_overlay_harness_name_refuses():
    """AC1-ERR sibling: a typo'd harness key refuses at the seam by name."""
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _inject(
            ["spawn", "-H", "claude", "--name", "w", "/target x"],
            err=err,
            profiles={"target": {"harness": {"codx": {"effort": "high"}}}},
        )
    assert exc.value.code == 2
    assert "agents.profiles.target.harness.codx" in err.getvalue()


@requires_rust
def test_lane_field_inside_overlay_refuses():
    """AC1-ERR: a ranking field in an overlay is a lane field, refused by
    name; nothing launches."""
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        _inject(
            ["spawn", "-H", "claude", "--name", "w", "hi"],
            err=err,
            harness={"codex": {"model": "opus"}},
        )
    assert exc.value.code == 2
    assert "agents.defaults.harness.codex.model" in err.getvalue()
    assert "lane field" in err.getvalue()


@requires_rust
def test_overlay_scoped_to_this_verbs_profile():
    """A typo in an unrelated verb's overlay must not block this dispatch."""
    out = _inject(
        ["spawn", "--name", "w", "/target x"],
        harness={"claude": {"effort": "high"}},
        profiles={"review": {"harness": {"codx": {"effort": "high"}}}},
    )
    assert "--effort" in out
