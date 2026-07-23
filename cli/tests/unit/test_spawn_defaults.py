"""US8 spawn-seam injector: config.agents.defaults -> argv (x-de9d).

Precedence explicit flag > config > builtin, resolved field-by-field. Provider
validated (exit 2 on a bad name); config-sourced effort degrades open on a
no-surface provider while an explicit --effort stays fail-closed downstream.
"""
from __future__ import annotations

import io

import pytest

from fno.agents.spawn_defaults import inject_spawn_defaults


class _Defaults:
    def __init__(self, provider="", model="", effort="", substrate="", permission_mode=""):
        self.provider = provider
        self.model = model
        self.effort = effort
        self.substrate = substrate
        self.permission_mode = permission_mode


class _Settings:
    def __init__(self, profiles=None, **kw):
        # profiles: {verb: {field: value}} -> {verb: _Defaults}
        prof = {k: _Defaults(**v) for k, v in (profiles or {}).items()}
        self.agents = type("A", (), {"defaults": _Defaults(**kw), "profiles": prof})()


def _inject(args, err=None, env=None, profiles=None, **cfg):
    return inject_spawn_defaults(
        args, settings=_Settings(profiles=profiles, **cfg), stderr=err, env=env or {}
    )


def test_non_spawn_verb_untouched():
    assert _inject(["ask", "w", "hi"], provider="codex") == ["ask", "w", "hi"]


def test_all_unset_is_noop():
    assert _inject(["spawn", "w", "hi"]) == ["spawn", "w", "hi"]


def test_ac3_bare_spawn_inherits_provider_and_model():
    # AC3-HP: bare spawn inherits both fields.
    out = _inject(["spawn", "w", "hi"], provider="codex", model="gpt-5.6-sol")
    assert out[0] == "spawn"
    assert "--provider" in out and out[out.index("--provider") + 1] == "codex"
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
        ["spawn", "-p", "claude", "w", "hi"],
        err=err,
        provider="codex",
        model="gpt-5.6-sol",
    )
    assert out.count("--provider") == 0  # no config provider injected
    assert "-p" in out  # the explicit flag survives
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
        _inject(["spawn", "w", "hi"], err=err, provider="cluade")
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
    out = _inject(["spawn", "-p", "gemini", "w"], err=err, effort="high")
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
    out = _inject(["spawn", "-p", "gemini", "--effort", "high", "w"], err=err, effort="low")
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
    assert "--provider" in out and out.index("--provider") < out.index("--argv")


def test_ac2_hp_codex_spawn_does_not_inherit_claude_model():
    # config model=opus (a claude alias), provider unset; an explicit -p codex
    # retargets the spawn. The claude model must NOT ride onto codex, and a
    # stderr line names the config model, its implied provider, and the resolved
    # one. env={} => resolve_dispatch_provider infers claude as the implied.
    err = io.StringIO()
    out = _inject(["spawn", "-p", "codex", "w"], err=err, env={}, model="opus")
    assert out.count("--model") == 0  # no --model opus injected
    assert "opus" not in out
    msg = err.getvalue()
    assert "opus" in msg and "codex" in msg  # names the model and resolved provider


def test_ac5_fr_provider_resolution_failure_degrades_open(monkeypatch):
    # If resolve_dispatch_provider raises, the model default must degrade to
    # injecting nothing rather than aborting the spawn.
    import fno.agents.provider_resolve as pr

    def _boom(*_a, **_k):
        raise RuntimeError("resolution exploded")

    monkeypatch.setattr(pr, "resolve_dispatch_provider", _boom)
    err = io.StringIO()
    # provider unset so the model branch must call resolve_dispatch_provider.
    out = _inject(["spawn", "-p", "codex", "w"], err=err, env={}, model="opus")
    assert out.count("--model") == 0  # nothing injected
    assert out[-1] == "w"  # spawn not aborted; positional preserved
    assert "resolution" in err.getvalue().lower() or "leaving" in err.getvalue().lower()


def test_ac6_edge_no_explicit_provider_injects_model_unchanged():
    # No explicit -p, config model=opus, provider unset: --model opus is injected
    # exactly as before, with no NEW skip/leave reason line. env={} => implied
    # provider (claude) == resolved provider (claude) => inject.
    err = io.StringIO()
    out = _inject(["spawn", "w"], err=err, env={}, model="opus")
    assert out == ["spawn", "--model", "opus", "w"]  # byte-identical to pre-fix
    # the "leaving model to the harness" skip line must NOT fire here
    assert "leaving model to the harness" not in err.getvalue()


def test_residual_ambient_codex_leaves_claude_model_to_harness():
    # x-0e29: no explicit -p, provider unset, but a CODEX-ambient marker. The
    # provider-less claude-shaped model (opus) must NOT ride onto the inferred
    # codex spawn (it 400s after the round-trip). home=claude != target=codex.
    err = io.StringIO()
    out = _inject(["spawn", "w"], err=err, env={"CODEX_THREAD_ID": "x"}, model="opus")
    assert out == ["spawn", "w"]  # no --model injected
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
        ["spawn", "worker1", "/blueprint x-1234"], err=err,
        provider="claude", effort="high",
        profiles={"blueprint": {"model": "fable"}},
    )
    assert "--model" in out and out[out.index("--model") + 1] == "fable"
    assert "--effort" in out and out[out.index("--effort") + 1] == "high"
    msg = err.getvalue()
    assert "model=agents.profiles.blueprint" in msg
    assert "effort=agents.defaults" in msg


def test_ac2_hp_substrate_and_permission_from_profile():
    out = _inject(
        ["spawn", "w", "/target x-9"],
        provider="claude",
        profiles={"target": {"substrate": "bg", "permission_mode": "yolo"}},
    )
    assert "--substrate" in out and out[out.index("--substrate") + 1] == "bg"
    assert "--permission-mode" in out and out[out.index("--permission-mode") + 1] == "yolo"


def test_ac2_hp_explicit_substrate_token_wins_permission_still_injects():
    # A trailing `pane` token pins substrate (normalized to --substrate pane);
    # only permission-mode is injected from the profile.
    out = _inject(
        ["spawn", "w", "/target x-9", "pane"],
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
            ["spawn", "w", seed], provider="claude",
            profiles={"think": {"model": "fable"}},
        )
        assert out[out.index("--model") + 1] == "fable", seed


def test_ac4_err_incompatible_config_substrate_degrades_open():
    # bg on a codex-resolved spawn: no --substrate injected, warning names it.
    err = io.StringIO()
    out = _inject(
        ["spawn", "-p", "codex", "w", "/think x"], err=err,
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
            ["spawn", "w", "/target x-1"], err=err,
            profiles={"target": {"provider": "banana"}},
        )
    assert exc.value.code == 2
    assert "agents.profiles.target.provider" in err.getvalue()


def test_ac5_err_nonmatching_seed_spawns_normally_under_bad_profile():
    # The same bad-provider profile does NOT fire for a /think seed.
    out = _inject(
        ["spawn", "w", "/think x"],
        profiles={"target": {"provider": "banana"}},
    )
    assert out == ["spawn", "w", "/think x"]


def test_ac6_edge_verb_not_first_token_no_profile():
    # AC6-EDGE: verb not first -> no key; only defaults inject.
    out = _inject(
        ["spawn", "w", "fix the /target docs"],
        provider="claude",
        profiles={"target": {"model": "opus"}},
    )
    assert "--model" not in out  # target profile never fired
    assert "--provider" in out  # defaults still applied


def test_ac6_edge_absolute_path_never_matches():
    out = _inject(
        ["spawn", "w", "/usr/bin/x is a path"],
        profiles={"usr": {"model": "opus"}},
    )
    assert "--model" not in out


def test_ac7_edge_explicit_flag_beats_profile_beats_defaults():
    # Explicit -m wins; without it, profile beats defaults.
    out1 = _inject(
        ["spawn", "-m", "haiku", "w", "/target x-1"],
        model="sonnet", profiles={"target": {"model": "opus"}},
    )
    assert out1.count("--model") == 0  # only the explicit -m
    assert "opus" not in out1 and "sonnet" not in out1

    out2 = _inject(
        ["spawn", "w", "/target x-1"],
        model="sonnet", profiles={"target": {"model": "opus"}},
    )
    assert out2[out2.index("--model") + 1] == "opus"


def test_uppercase_verb_no_key():
    # Deliberate: the verb surface is lowercase; /Target does not match.
    out = _inject(
        ["spawn", "w", "/Target x-1"],
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
    _inject(["spawn", "w", "/target x"], err=err, profiles={"other": {"model": "x"}})
    assert "applied" not in err.getvalue()


def test_unknown_config_substrate_degrades_open():
    # An unknown substrate value is never injected (it would exit 2 at the spawn
    # parser); it degrades open with an "unknown substrate" warning.
    err = io.StringIO()
    out = _inject(
        ["spawn", "w", "/target x"], err=err, provider="claude",
        profiles={"target": {"substrate": "banana"}},
    )
    assert "--substrate" not in out
    assert "unknown substrate" in err.getvalue()


def test_permission_mode_skipped_on_nonclaude_headless():
    # codex headless cannot honor a mapped --permission-mode (its one-shot lane
    # hardcodes its own bypass and exits 2); the config value degrades open.
    err = io.StringIO()
    out = _inject(
        ["spawn", "-p", "codex", "--headless", "w", "/target x"], err=err,
        profiles={"target": {"permission_mode": "yolo"}},
    )
    assert "--permission-mode" not in out
    assert "permission-mode skipped" in err.getvalue()


def test_permission_mode_ok_on_nonclaude_pane():
    # The pane lane maps every provider, so codex+pane honors a mapped value.
    out = _inject(
        ["spawn", "-p", "codex", "w", "/target x", "pane"],
        profiles={"target": {"permission_mode": "yolo"}},
    )
    assert out[out.index("--permission-mode") + 1] == "yolo"


def test_permission_mode_injected_on_bare_nonclaude_spawn_pane_default():
    # No explicit substrate: `fno agents spawn` defaults to PANE (not the
    # autonomous headless default), which maps codex permission modes - so the
    # configured value must be injected, not skipped as incompatible.
    out = _inject(
        ["spawn", "-p", "codex", "w", "/target x"],
        profiles={"target": {"permission_mode": "yolo"}},
    )
    assert out[out.index("--permission-mode") + 1] == "yolo"


def test_explicit_yolo_suppresses_config_permission_mode():
    # --yolo/-Y is the same knob as --permission-mode (mutually exclusive
    # downstream); an explicit yolo must win, so no config value is injected.
    for flag in ("--yolo", "-Y"):
        out = _inject(
            ["spawn", flag, "w", "/target x"], provider="claude",
            profiles={"target": {"permission_mode": "bypassPermissions"}},
        )
        assert "--permission-mode" not in out, flag
