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
    def __init__(self, provider="", model="", effort=""):
        self.provider = provider
        self.model = model
        self.effort = effort


class _Settings:
    def __init__(self, **kw):
        self.agents = type("A", (), {"defaults": _Defaults(**kw)})()


def _inject(args, err=None, env=None, **cfg):
    return inject_spawn_defaults(
        args, settings=_Settings(**cfg), stderr=err, env=env or {}
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
    # codex has an effort surface but does NOT support "xhigh"; a config-sourced
    # value must skip + notice, never hard-fail the bare spawn.
    err = io.StringIO()
    out = _inject(["spawn", "w"], err=err, provider="codex", effort="xhigh")
    assert "--effort" not in out
    assert "effort skipped" in err.getvalue()
    assert "xhigh" in err.getvalue()


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
