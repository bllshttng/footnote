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


# ---------------------------------------------------------------------------
# The durable-grant coupling: arming the dispatch grant implies a live watcher
# ---------------------------------------------------------------------------


def _stub_grant_coupling(monkeypatch):
    """Stub the watcher-activation seam the grant coupling calls (its own
    import path, distinct from the enabled-coupling stubs above)."""
    import fno.pr_watch.cli as pwcli

    calls = {"activate": 0}

    def _act() -> str:
        calls["activate"] += 1
        return "activated"

    monkeypatch.setattr(pwcli, "ensure_watcher_activated", _act)
    return calls


def test_armed_dispatch_grant_activates_the_watcher(tmp_home, monkeypatch):
    calls = _stub_grant_coupling(monkeypatch)
    from fno.config_cli import app

    runner = CliRunner()
    r = runner.invoke(app, ["set", "config.auto_merge.enabled", "true"])
    assert r.exit_code == 0, r.output
    # enabled alone (grant defaults to none) arms nothing: a standing grant is
    # enabled AND grant=dispatch.
    assert calls["activate"] == 0
    r = runner.invoke(app, ["set", "config.auto_merge.grant", "dispatch"])
    assert r.exit_code == 0, r.output
    assert calls["activate"] == 1
    assert "activated to serve the standing dispatch grant" in r.output


def test_revoking_the_grant_never_deactivates(tmp_home, monkeypatch):
    """The coupling is one-way: pr_watch.enabled owns the off switch, and the
    watcher serves review dispatch beyond grants."""
    calls = _stub_grant_coupling(monkeypatch)
    import fno.pr_watch.cli as pwcli

    monkeypatch.setattr(pwcli, "deactivate_watcher", lambda: "unloaded")
    from fno.config_cli import app

    runner = CliRunner()
    runner.invoke(app, ["set", "config.auto_merge.enabled", "true"])
    runner.invoke(app, ["set", "config.auto_merge.grant", "dispatch"])
    calls["activate"] = 0
    r = runner.invoke(app, ["set", "config.auto_merge.grant", "none"])
    assert r.exit_code == 0, r.output
    assert calls["activate"] == 0


def test_panic_switch_blocks_grant_activation(tmp_home, monkeypatch):
    """Subordinate to autonomy.enabled: with the panic switch off, arming the
    grant must not bring an autonomous merger to life behind its back."""
    calls = _stub_grant_coupling(monkeypatch)
    from fno.config_cli import app

    runner = CliRunner()
    runner.invoke(app, ["set", "config.autonomy.enabled", "false"])
    runner.invoke(app, ["set", "config.auto_merge.enabled", "true"])
    r = runner.invoke(app, ["set", "config.auto_merge.grant", "dispatch"])
    assert r.exit_code == 0, r.output
    assert calls["activate"] == 0


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
    assert load_settings().pr_watch.enabled is True


def test_disable_unload_failure_is_loud(tmp_home, monkeypatch):
    """A failed unload on disable warns loudly - a still-ticking watcher must not go silent."""
    import fno.pr_watch.cli as pwcli

    monkeypatch.setattr(pwcli, "ensure_watcher_activated", lambda: "activated")
    monkeypatch.setattr(pwcli, "deactivate_watcher", lambda: "unload-failed")
    from fno.config_cli import app

    r = CliRunner().invoke(app, ["set", "config.pr_watch.enabled", "false"])
    assert r.exit_code == 0, r.output
    assert "WARNING" in r.output
    assert "may still be running" in r.output


def test_disable_unload_failure_diagnoses_before_uninstall(tmp_home, monkeypatch):
    """x-d19e: the warning used to lead with `pr watch uninstall` while the
    agent may still be running. It must name the check first, and frame
    uninstall as the deliberate removal it is, not the way to silence it."""
    import fno.pr_watch.cli as pwcli

    monkeypatch.setattr(pwcli, "ensure_watcher_activated", lambda: "activated")
    monkeypatch.setattr(pwcli, "deactivate_watcher", lambda: "unload-failed")
    from fno.config_cli import app

    r = CliRunner().invoke(app, ["set", "config.pr_watch.enabled", "false"])
    assert r.exit_code == 0, r.output
    out = r.output
    assert "launchctl list | grep sh.fno.pr-watcher" in out
    assert "retry the unload" in out
    assert "pr watch uninstall" in out
    assert "not to silence this warning" in out
    # The diagnostic precedes the uninstall in the text an agent reads.
    assert out.index("launchctl list") < out.index("pr watch uninstall")


def test_unrelated_key_does_not_touch_watcher(tmp_home, monkeypatch):
    calls = _stub_coupling(monkeypatch)
    from fno.config_cli import app

    r = CliRunner().invoke(app, ["set", "config.pr_watch.interval_seconds", "300"])
    assert r.exit_code == 0, r.output
    assert "activate" not in calls and "deactivate" not in calls
