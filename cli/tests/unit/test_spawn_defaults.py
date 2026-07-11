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


def test_ac3_explicit_provider_wins_model_still_inherited():
    # AC3-HP tail: -p claude wins field-by-field, model still inherited.
    out = _inject(
        ["spawn", "-p", "claude", "w", "hi"], provider="codex", model="gpt-5.6-sol"
    )
    assert out.count("--provider") == 0  # no config provider injected
    assert "-p" in out  # the explicit flag survives
    assert "--model" in out and out[out.index("--model") + 1] == "gpt-5.6-sol"


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
