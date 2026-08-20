"""US8 spawn-seam injector: config.agents.defaults -> argv (x-de9d).

Precedence explicit flag > config > builtin, resolved field-by-field. Provider
validated (exit 2 on a bad name); config-sourced effort degrades open on a
no-surface provider while an explicit --effort stays fail-closed downstream.
"""
from __future__ import annotations

import io

import pytest

from fno.agents.spawn_defaults import inject_spawn_defaults, resolve_lane_vendor


class _Defaults:
    def __init__(self, provider="", model="", effort="", substrate="", permission_mode="",
                 route="", account="", pane_group="", lanes=None):
        self.provider = provider
        self.model = model
        self.effort = effort
        self.substrate = substrate
        self.permission_mode = permission_mode
        self.route = route
        self.account = account
        self.pane_group = pane_group
        self.lanes = [
            _Defaults(**lane) if isinstance(lane, dict) else lane
            for lane in (lanes or [])
        ]


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


def test_non_spawn_verb_untouched():
    assert _inject(["ask", "w", "hi"], provider="codex") == ["ask", "w", "hi"]


def test_all_unset_is_noop():
    assert _inject(["spawn", "--name", "w", "hi"]) == ["spawn", "--name", "w", "hi"]


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


def test_config_effort_unmappable_for_provider_degrades_open():
    # codex has an effort surface but does NOT support "max"; a config-sourced
    # value must skip + notice, never hard-fail the bare spawn.
    err = io.StringIO()
    out = _inject(["spawn", "w"], err=err, provider="codex", effort="max")
    assert "--effort" not in out
    assert "effort skipped" in err.getvalue()
    assert "max" in err.getvalue()


def test_config_effort_unknown_value_degrades_open():
    # A garbage config effort value never reaches the fail-closed validator.
    err = io.StringIO()
    out = _inject(["spawn", "w"], err=err, provider="claude", effort="banana")
    assert "--effort" not in out
    assert "effort skipped" in err.getvalue()


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
    # one. env={} => resolve_dispatch_provider infers claude as the implied.
    err = io.StringIO()
    out = _inject(["spawn", "-H", "codex", "w"], err=err, env={}, model="opus")
    assert out.count("--model") == 0  # no --model opus injected
    assert "opus" not in out
    msg = err.getvalue()
    assert "opus" in msg and "codex" in msg  # names the model and resolved provider


def test_ac5_fr_provider_resolution_failure_degrades_open(monkeypatch):
    # If resolve_dispatch_provider raises, the model default must degrade to
    # injecting nothing rather than aborting the spawn.
    import fno.dispatch_flags as pr

    def _boom(*_a, **_k):
        raise RuntimeError("resolution exploded")

    monkeypatch.setattr(pr, "resolve_dispatch_provider", _boom)
    err = io.StringIO()
    # provider unset so the model branch must call resolve_dispatch_provider.
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


def test_profile_lanes_round_robin_from_live_row_count(monkeypatch):
    import fno.agents.spawn_defaults as spawn_defaults

    lanes = [
        _lane("codex", effort="high", substrate="pane", permission_mode="yolo"),
        _lane("claude", route="zai/glm-5.3[1m]", substrate="bg"),
    ]
    for live_count, expected_harness, expected_rung in (
        (0, "codex", "lanes[0]"),
        (1, "claude", "lanes[1]"),
        (2, "codex", "lanes[0]"),
        (3, "claude", "lanes[1]"),
    ):
        monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda n=live_count: [object()] * n)
        err = io.StringIO()
        out = _inject(
            ["spawn", "--name", f"w{live_count}", "/fno:target x-1"],
            err=err,
            profiles={"target": {"lanes": lanes}},
        )
        assert out[out.index("--harness") + 1] == expected_harness
        assert expected_rung in err.getvalue()


def test_profile_lanes_skip_capped_vendor(monkeypatch):
    import fno.agents.spawn_defaults as spawn_defaults
    import fno.agents.spawn_gate as spawn_gate

    monkeypatch.setattr(spawn_defaults, "_read_registry_rows", lambda: [object()])
    monkeypatch.setattr(spawn_gate, "provider_live_count", lambda vendor: 2)
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"],
        err=err,
        max_lanes={"zai": 2},
        profiles={"target": {"lanes": [
            _lane("codex", permission_mode="yolo"),
            _lane("claude", route="zai/glm-5.3[1m]", substrate="bg"),
        ]}},
    )
    assert out[out.index("--harness") + 1] == "codex"
    assert "zai lane skipped at 2 of 2" in err.getvalue()
    assert "agents.profiles.target.lanes[0]" in err.getvalue()


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
    # bg on a codex-resolved spawn: no --substrate injected, warning names it.
    err = io.StringIO()
    out = _inject(
        ["spawn", "-H", "codex", "--name", "w", "/think x"], err=err,
        profiles={"think": {"substrate": "bg"}},
    )
    assert "--substrate" not in out
    msg = err.getvalue()
    assert "substrate skipped" in msg
    assert "bg" in msg and "codex" in msg
    assert "agents.profiles.think.substrate" in msg


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
    # The same bad-provider profile does NOT fire for a /think seed.
    out = _inject(
        ["spawn", "--name", "w", "/think x"],
        profiles={"target": _lane("banana")},
    )
    assert out == ["spawn", "--name", "w", "/think x"]


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
    # A spawn with zero injected fields prints no `applied` line.
    err = io.StringIO()
    _inject(["spawn", "--name", "w", "/target x"], err=err, profiles={"other": {"model": "x"}})
    assert "applied" not in err.getvalue()


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
        # bg is claude-only, so a codex spawn degrades open (warn, skip).
        assert "--substrate" not in out, f"{flag}: bg must not be injected for codex"

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


# ---------------------------------------------------------------------------
# The fallback chain (AC5-*)
# ---------------------------------------------------------------------------


class _ChainSettings:
    def __init__(self, agents):
        self.agents = agents


def _chain_settings(table):
    from fno.config import AgentsBlock

    return _ChainSettings(AgentsBlock(fallback=table))


_OPERATOR_RULE = {
    "S": [
        {"harness": "claude", "model": "sonnet", "substrate": "bg"},
        {"harness": "codex", "model": "gpt-5.6-sol", "effort": "medium"},
    ],
    "L": [
        {"harness": "codex", "model": "gpt-5.6-sol", "effort": "high"},
        {"harness": "claude", "model": "sonnet", "substrate": "bg"},
    ],
    "default": [
        {"harness": "claude", "model": "sonnet", "substrate": "bg"},
        {"harness": "codex", "model": "gpt-5.6-sol", "effort": "high"},
    ],
}


class TestResolveFallbackChain:
    def test_ac5_hp_size_picks_the_operators_own_split(self, monkeypatch) -> None:
        # "Simple work goes to a claude sonnet background thread, complex work
        # goes to codex" - the sentence, executable.
        from fno.agents import spawn_defaults as sd

        monkeypatch.setattr(sd, "link_is_exhausted", lambda link, now=None, repo_root=None: False)
        st = _chain_settings(_OPERATOR_RULE)

        assert sd.link_id(sd.resolve_fallback_chain("L", settings=st)[0]) == (
            "codex/gpt-5.6-sol"
        )
        assert sd.link_id(sd.resolve_fallback_chain("S", settings=st)[0]) == (
            "claude/sonnet"
        )

    def test_every_size_has_more_than_one_link(self, monkeypatch) -> None:
        # A chain with one link is not a chain: a claude weekly cap and a z.ai
        # five-hour cap are different meters, and either can be the one down.
        from fno.agents import spawn_defaults as sd

        monkeypatch.setattr(sd, "link_is_exhausted", lambda link, now=None, repo_root=None: False)
        st = _chain_settings(_OPERATOR_RULE)
        for size in ("S", "L", "M"):
            assert len(sd.resolve_fallback_chain(size, settings=st)) >= 2, size

    def test_an_absent_size_reads_default(self, monkeypatch) -> None:
        from fno.agents import spawn_defaults as sd

        monkeypatch.setattr(sd, "link_is_exhausted", lambda link, now=None, repo_root=None: False)
        st = _chain_settings(_OPERATOR_RULE)
        assert sd.link_id(sd.resolve_fallback_chain("M", settings=st)[0]) == (
            "claude/sonnet"
        )
        assert sd.link_id(sd.resolve_fallback_chain(None, settings=st)[0]) == (
            "claude/sonnet"
        )

    def test_an_absent_table_yields_no_spawns(self) -> None:
        from fno.agents import spawn_defaults as sd

        assert sd.resolve_fallback_chain("L", settings=_chain_settings({})) == []

    def test_ac5_edge_an_exhausted_link_is_skipped(self, monkeypatch) -> None:
        from fno.agents import spawn_defaults as sd

        monkeypatch.setattr(
            sd, "link_is_exhausted",
            lambda link, now=None, repo_root=None: sd.link_id(link) == "codex/gpt-5.6-sol",
        )
        chain = sd.resolve_fallback_chain("L", settings=_chain_settings(_OPERATOR_RULE))
        assert [sd.link_id(x) for x in chain] == ["claude/sonnet"]

    def test_ac5_edge_an_all_exhausted_chain_returns_empty(self, monkeypatch) -> None:
        # NOT link zero. Routing into a known-capped provider is worse than
        # holding, and holding is what an empty chain makes the caller do.
        from fno.agents import spawn_defaults as sd

        monkeypatch.setattr(sd, "link_is_exhausted", lambda link, now=None, repo_root=None: True)
        assert sd.resolve_fallback_chain(
            "L", settings=_chain_settings(_OPERATOR_RULE)
        ) == []

    def test_an_already_spent_link_is_not_offered_again(self, monkeypatch) -> None:
        from fno.agents import spawn_defaults as sd

        monkeypatch.setattr(sd, "link_is_exhausted", lambda link, now=None, repo_root=None: False)
        chain = sd.resolve_fallback_chain(
            "L", exclude=["codex/gpt-5.6-sol"],
            settings=_chain_settings(_OPERATOR_RULE),
        )
        assert [sd.link_id(x) for x in chain] == ["claude/sonnet"]


class TestChainValidatorRefuses:
    """AC5-NEG: the failover path is where degrading open costs money.

    The refusal lives on the READ, not on the load. A field validator would
    fail load_settings() process-wide, so one typo would make every fno command
    raise and kill the pr-watch tick at its settings phase - taking down the
    daemon that runs the failover, which is worse than the mis-spawn it
    prevents.
    """

    def _refuses(self, table):
        import pytest as _pytest

        from fno.agents.spawn_defaults import (
            FallbackConfigError,
            resolve_fallback_chain,
        )

        with _pytest.raises(FallbackConfigError) as exc:
            resolve_fallback_chain("L", settings=_chain_settings(table))
        return str(exc.value)

    def test_an_out_of_enum_harness_is_refused_by_name(self) -> None:
        message = self._refuses({"L": [{"harness": "banana", "model": "x"}]})
        assert "banana" in message
        assert "agents.fallback.L[0].harness" in message

    def test_an_unknown_size_key_is_refused_by_name(self) -> None:
        assert "agents.fallback.XL" in self._refuses({"XL": [{"harness": "codex"}]})

    def test_a_non_list_chain_is_refused(self) -> None:
        assert "agents.fallback.L" in self._refuses({"L": "codex"})

    def test_a_non_table_link_is_refused(self) -> None:
        assert "agents.fallback.L[0]" in self._refuses({"L": ["codex"]})

    def test_a_malformed_chain_never_fails_the_config_load(self) -> None:
        # The whole point of moving the refusal: every one of these loads.
        from fno.config import AgentsBlock

        for bad in (
            {"XL": [{"harness": "codex"}]},
            {"L": "codex"},
            {"L": ["codex"]},
            {"L": [{"harness": "banana"}]},
            "banana",
        ):
            AgentsBlock(fallback=bad)

    def test_the_profiles_table_still_degrades_open(self) -> None:
        # The contrast that makes the refusal deliberate: a typo in profiles
        # must never brick ordinary spawning.
        from fno.config import AgentsBlock

        assert AgentsBlock(profiles="banana").profiles == {}

    def test_an_empty_table_is_legal(self) -> None:
        from fno.config import AgentsBlock

        assert AgentsBlock(fallback={}).fallback == {}


class TestLinkToSpawnFlags:
    def test_no_new_axis_vocabulary(self) -> None:
        from fno.agents import spawn_defaults as sd

        codex, claude = sd.validate_fallback(_OPERATOR_RULE)["L"]
        assert sd.link_to_spawn_flags(codex) == [
            "-H", "codex", "-m", "gpt-5.6-sol", "--effort", "high",
            "--substrate", "pane",
        ]
        assert sd.link_to_spawn_flags(claude) == [
            "-H", "claude", "-m", "sonnet", "--substrate", "bg",
        ]

    def test_a_codex_link_defaults_to_a_pane_not_bg(self) -> None:
        # The Rust client rejects --substrate bg for a non-claude harness, so
        # this is a real difference in the spawn call, not a naming change.
        from fno.agents import spawn_defaults as sd

        link = sd.validate_fallback({"L": [{"harness": "codex", "model": "m"}]})["L"][0]
        flags = sd.link_to_spawn_flags(link)
        assert "--substrate" in flags
        assert flags[flags.index("--substrate") + 1] == "pane"

    def test_effort_alone_does_not_make_two_links_one_destination(self) -> None:
        # Retrying the same vendor at a different reasoning setting does not
        # answer a cap, so the walk's memory key ignores effort.
        from fno.agents import spawn_defaults as sd

        a, b = sd.validate_fallback({"L": [
            {"harness": "codex", "model": "m", "effort": "high"},
            {"harness": "codex", "model": "m", "effort": "low"},
        ]})["L"]
        assert sd.link_id(a) == sd.link_id(b)


class TestLinkIdentityIncludesTheAccountAxis:
    def test_two_accounts_on_one_model_are_two_destinations(self) -> None:
        # An account names a different bill and a different meter. Folding two
        # links together spends the first and skips the second as spent.
        from fno.agents import spawn_defaults as sd

        a, b = sd.validate_fallback({"L": [
            {"harness": "claude", "model": "sonnet", "account": "primary"},
            {"harness": "claude", "model": "sonnet", "account": "secondary"},
        ]})["L"]
        assert sd.link_id(a) != sd.link_id(b)

    def test_an_unpinned_link_keeps_the_bare_identity(self) -> None:
        from fno.agents import spawn_defaults as sd

        link = sd.validate_fallback({"L": [{"harness": "codex", "model": "m"}]})["L"][0]
        assert sd.link_id(link) == "codex/m"

    def test_the_second_account_is_still_offered_after_the_first(
        self, monkeypatch
    ) -> None:
        from fno.agents import spawn_defaults as sd

        monkeypatch.setattr(
            sd, "link_is_exhausted",
            lambda link, now=None, repo_root=None: False, raising=True,
        )
        table = {"L": [
            {"harness": "claude", "model": "sonnet", "account": "primary"},
            {"harness": "claude", "model": "sonnet", "account": "secondary"},
        ]}
        chain = sd.resolve_fallback_chain(
            "L", exclude=["claude/sonnet@primary"],
            settings=_chain_settings(table),
        )
        assert [sd.link_id(x) for x in chain] == ["claude/sonnet@secondary"]


class TestForeignProjectRooting:
    def test_the_chain_is_read_from_the_candidates_repo(self, monkeypatch, tmp_path):
        # The recovery roster is global. A foreign worker resolving the
        # daemon's chain would spawn on a vendor its own project never
        # authorized.
        from fno.agents import spawn_defaults as sd

        seen = {}

        def _for_repo(root):
            seen["root"] = root
            return _chain_settings(_OPERATOR_RULE)

        monkeypatch.setattr(
            "fno.config.load_settings_for_repo", _for_repo, raising=True
        )
        monkeypatch.setattr(
            sd, "link_is_exhausted",
            lambda link, now=None, repo_root=None: False, raising=True,
        )
        sd.resolve_fallback_chain("L", repo_root=str(tmp_path))
        assert str(seen["root"]) == str(tmp_path)

    def test_headroom_is_read_from_the_candidates_repo(self, monkeypatch, tmp_path):
        from fno.agents import spawn_defaults as sd

        seen = {}

        class _V:
            state = object()

        monkeypatch.setattr(
            sd, "_harness_records",
            lambda harness, repo_root=None: [type("R", (), {"id": "acct"})()],
            raising=True,
        )

        def _headroom(pid, now=None, repo_root=None):
            seen["root"] = repo_root
            return _V()

        monkeypatch.setattr(
            "fno.adapters.providers.runtime_state.headroom", _headroom, raising=True
        )
        link = sd.validate_fallback({"L": [{"harness": "codex", "model": "m"}]})["L"][0]
        sd.link_is_exhausted(link, repo_root=str(tmp_path))
        assert str(seen["root"]) == str(tmp_path)


# --- model-implies-vendor mismatch warning (change 5, spawn half) ------------


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


def test_lane_vendor_resolves_unrouted_harness_from_final_argv():
    assert resolve_lane_vendor(["codex", "-C", "/tmp/workspace"]) == "openai"


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
    assert emitted == [
        (
            "model_vendor_mismatch",
            {
                "model": "opus",
                "implied_vendor": "anthropic",
                "resolved_vendor": "openai",
            },
        )
    ]


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
    assert "zai lane skipped at 2 of 2" in err.getvalue()

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


def test_lane_validation_refusals_run_on_real_dict_lanes(monkeypatch):
    """Live config lanes arrive as raw TOML dicts, not objects. Every other lane
    test builds objects, which take the getattr branch, so the Mapping-only
    unknown-field and non-string refusals were never executed."""
    import fno.agents.spawn_defaults as spawn_defaults

    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        spawn_defaults._validated_lanes(
            [_lane("claude", nonsense="x")], "agents.profiles.target.lanes", err
        )
    assert exc.value.code == 2
    assert "unknown field 'nonsense'" in err.getvalue()

    err2 = io.StringIO()
    with pytest.raises(SystemExit) as exc2:
        spawn_defaults._validated_lanes(
            [{"provider": 7}], "agents.profiles.target.lanes", err2
        )
    assert exc2.value.code == 2
    assert "must be a string" in err2.getvalue()

    err3 = io.StringIO()
    with pytest.raises(SystemExit) as exc3:
        spawn_defaults._validated_lanes([{}], "agents.profiles.target.lanes", err3)
    assert exc3.value.code == 2
    assert "is empty" in err3.getvalue()


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


def test_config_pane_group_still_injects_without_a_conflicting_flag(monkeypatch):
    err = io.StringIO()
    out = _inject(
        ["spawn", "--name", "w", "/fno:target x-1"],
        err=err,
        profiles={"target": _lane("codex", substrate="pane", pane_group="codex")},
    )
    assert out[out.index("--tab") + 1] == "codex"


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
