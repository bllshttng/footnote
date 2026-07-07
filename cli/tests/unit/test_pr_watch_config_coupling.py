"""x-e106: `fno config set pr_watch.enabled` couples to the launchd agent.

Setting enabled=true installs+loads the watcher; enabled=false unloads it.
Activation failure is loud and never reverts config (doctor is the guard).
The launchctl side is stubbed - these tests assert the coupling fires, not
that launchd works.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner


@pytest.fixture()
def tmp_home(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / ".fno").mkdir()
    try:
        from fno.config import load_settings
        load_settings.cache_clear()
    except Exception:
        pass
    return tmp_path


def _stub_coupling(monkeypatch, activate_outcome="activated"):
    import fno.pr_watch.cli as pwcli

    calls: dict[str, object] = {}

    def _act() -> str:
        calls["activate"] = True
        return activate_outcome

    def _deact() -> str:
        calls["deactivate"] = True
        return "unloaded"

    monkeypatch.setattr(pwcli, "ensure_watcher_activated", _act)
    monkeypatch.setattr(pwcli, "deactivate_watcher", _deact)
    return calls


def test_enable_triggers_activation(tmp_home, monkeypatch):
    calls = _stub_coupling(monkeypatch)
    from fno.config_cli import app

    r = CliRunner().invoke(app, ["set", "config.pr_watch.enabled", "true"])
    assert r.exit_code == 0, r.output
    assert calls.get("activate") is True
    assert "installed and loaded" in r.output


def test_disable_triggers_unload(tmp_home, monkeypatch):
    calls = _stub_coupling(monkeypatch)
    from fno.config_cli import app

    r = CliRunner().invoke(app, ["set", "config.pr_watch.enabled", "false"])
    assert r.exit_code == 0, r.output
    assert calls.get("deactivate") is True
    assert "disabled" in r.output


def test_activation_failure_is_loud_and_keeps_config(tmp_home, monkeypatch):
    """AC1-ERR: a launchctl failure warns loudly; the enable still stuck."""
    _stub_coupling(monkeypatch, activate_outcome="load-failed")
    from fno.config_cli import app

    r = CliRunner().invoke(app, ["set", "config.pr_watch.enabled", "true"])
    assert r.exit_code == 0, r.output
    assert "WARNING" in r.output
    assert "activation failed" in r.output

    # Config value stuck despite the activation failure.
    from fno.config import load_settings
    load_settings.cache_clear()
    assert load_settings().config.pr_watch.enabled is True


def test_unrelated_key_does_not_touch_watcher(tmp_home, monkeypatch):
    calls = _stub_coupling(monkeypatch)
    from fno.config_cli import app

    r = CliRunner().invoke(app, ["set", "config.pr_watch.interval_seconds", "300"])
    assert r.exit_code == 0, r.output
    assert "activate" not in calls and "deactivate" not in calls
