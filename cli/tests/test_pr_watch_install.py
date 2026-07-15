"""Tests for fno pr-watch CLI surface and plist installer.

TDD: tests written BEFORE implementation.  Every test targets a named
acceptance criterion from the task 1.3 spec.

No real ~/Library/LaunchAgents write, no real launchctl load, no real
claude/gh.  All I/O is redirected to tmp directories.
"""
from __future__ import annotations

import json
import os
import textwrap
from pathlib import Path
from typing import Any

import pytest


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture()
def tmp_home(tmp_path, monkeypatch):
    """Redirect HOME + state dir to a tmp directory for isolation."""
    home = tmp_path / "home"
    home.mkdir()
    fno_dir = home / ".fno"
    fno_dir.mkdir()
    monkeypatch.setenv("HOME", str(home))
    # Clear load_settings cache so config reads fresh from the tmp HOME
    try:
        from fno.config import load_settings
        load_settings.cache_clear()
    except Exception:
        pass
    yield home


@pytest.fixture()
def tmp_launch_agents(tmp_path):
    """Return a temp dir standing in for ~/Library/LaunchAgents."""
    d = tmp_path / "LaunchAgents"
    d.mkdir()
    return d


@pytest.fixture()
def plist_kwargs(tmp_launch_agents):
    """Common kwargs for render_plist / install with a known LaunchAgents dir."""
    return {
        "launch_agents_dir": tmp_launch_agents,
        "fno_binary": "/usr/local/bin/fno",
        "install_path": str(Path(os.environ.get("PATH", "/usr/bin:/bin"))),
    }


# ---------------------------------------------------------------------------
# Helper: import _install lazily (module may not exist yet in RED phase)
# ---------------------------------------------------------------------------


def _install():
    from fno.pr_watch import _install as m
    return m


# ---------------------------------------------------------------------------
# AC3-HP: render_plist returns a string with the right shape
# ---------------------------------------------------------------------------


def test_ac3hp_render_plist_contains_required_keys(tmp_home, plist_kwargs):
    """render_plist() returns a valid XML plist string containing required keys."""
    m = _install()
    rendered = m.render_plist(**plist_kwargs)

    assert "sh.fno.pr-watcher" in rendered
    assert "fno" in rendered
    assert "pr-watch" in rendered
    assert "tick" in rendered
    assert "<false/>" in rendered  # RunAtLoad false


def test_ac3hp_install_prints_plist_before_writing(
    tmp_home, tmp_launch_agents, capsys, monkeypatch
):
    """install() prints the full plist text before writing (confirmed path)."""
    m = _install()

    # Simulate user confirmation
    monkeypatch.setattr("typer.confirm", lambda *a, **kw: True)

    m.install(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
        dry_run=False,
    )

    captured = capsys.readouterr()
    # Plist content must appear in stdout
    assert "sh.fno.pr-watcher" in captured.out
    assert "<false/>" in captured.out  # RunAtLoad=false


def test_ac3hp_install_writes_file_on_confirm(
    tmp_home, tmp_launch_agents, monkeypatch
):
    """install() writes the plist file when user confirms."""
    m = _install()
    monkeypatch.setattr("typer.confirm", lambda *a, **kw: True)

    m.install(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
        dry_run=False,
    )

    plist_path = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    assert plist_path.exists(), "plist file should be written after confirm"
    content = plist_path.read_text()
    assert "sh.fno.pr-watcher" in content


def test_ac3hp_dry_run_prints_plist_writes_nothing(
    tmp_home, tmp_launch_agents, capsys
):
    """--dry-run prints the plist and writes nothing."""
    m = _install()

    m.install(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
        dry_run=True,
    )

    captured = capsys.readouterr()
    assert "sh.fno.pr-watcher" in captured.out

    plist_path = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    assert not plist_path.exists(), "dry-run must not write the plist file"


# ---------------------------------------------------------------------------
# AC3-ERR: confirm=no -> no file, non-zero exit, message "not installed"
# ---------------------------------------------------------------------------


def test_ac3err_decline_writes_nothing(tmp_home, tmp_launch_agents, monkeypatch):
    """Declining the confirm prompt writes no file and exits with SystemExit."""
    m = _install()
    monkeypatch.setattr("typer.confirm", lambda *a, **kw: False)

    with pytest.raises(SystemExit) as exc_info:
        m.install(
            launch_agents_dir=tmp_launch_agents,
            fno_binary="/usr/local/bin/fno",
            install_path="/usr/bin:/bin",
            dry_run=False,
        )

    assert exc_info.value.code != 0

    plist_path = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    assert not plist_path.exists(), "declined install must not write the plist"


def test_ac3err_decline_message_contains_not_installed(
    tmp_home, tmp_launch_agents, capsys, monkeypatch
):
    """Declining shows a message containing 'not installed'."""
    m = _install()
    monkeypatch.setattr("typer.confirm", lambda *a, **kw: False)

    with pytest.raises(SystemExit):
        m.install(
            launch_agents_dir=tmp_launch_agents,
            fno_binary="/usr/local/bin/fno",
            install_path="/usr/bin:/bin",
            dry_run=False,
        )

    captured = capsys.readouterr()
    assert "not installed" in (captured.out + captured.err).lower()


# ---------------------------------------------------------------------------
# AC3-EDGE: plist security and correctness checks
# ---------------------------------------------------------------------------


def test_ac3edge_plist_has_path_and_home_no_api_key(tmp_home, plist_kwargs):
    """Rendered plist has PATH and HOME in EnvironmentVariables; no ANTHROPIC_API_KEY."""
    m = _install()
    rendered = m.render_plist(**plist_kwargs)

    assert "<key>PATH</key>" in rendered
    assert "<key>HOME</key>" in rendered
    assert "ANTHROPIC_API_KEY" not in rendered


def test_ac3edge_run_at_load_is_false(tmp_home, plist_kwargs):
    """RunAtLoad is explicitly false in the rendered plist."""
    m = _install()
    rendered = m.render_plist(**plist_kwargs)

    # The false element must appear after the RunAtLoad key
    idx_key = rendered.find("<key>RunAtLoad</key>")
    assert idx_key >= 0, "RunAtLoad key not found"
    idx_false = rendered.find("<false/>", idx_key)
    assert idx_false > idx_key, "RunAtLoad must be set to <false/>"


def test_ac3edge_xml_escape_in_paths(tmp_home, tmp_launch_agents):
    """Special XML characters in PATH are properly escaped."""
    m = _install()
    # Ampersand would be a pathological PATH; xml_escape should handle it
    rendered = m.render_plist(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin&weird",
    )
    # Ampersand must be escaped as &amp;
    assert "&amp;" in rendered
    assert "&weird" not in rendered  # raw & must not appear in attribute context


# ---------------------------------------------------------------------------
# AC3-FR: uninstall removes plist but preserves watermark store
# ---------------------------------------------------------------------------


def test_ac3fr_uninstall_removes_plist_preserves_watermark(
    tmp_home, tmp_launch_agents, monkeypatch
):
    """uninstall() removes the plist file but leaves the watermark store intact."""
    m = _install()

    # Pre-seed a plist file
    plist_path = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    plist_path.write_text("<plist/>")

    # Pre-seed a watermark store in the tmp HOME's .fno dir
    state_file = tmp_home / ".fno" / "pr-watcher-state.json"
    state_file.write_text(json.dumps({"some/repo#1": {"parked": None}}))

    # Stub launchctl so we don't call the real one
    monkeypatch.setattr(m, "_run_launchctl", lambda *a, **kw: 0)

    m.uninstall(launch_agents_dir=tmp_launch_agents)

    assert not plist_path.exists(), "plist should be removed by uninstall"
    assert state_file.exists(), "watermark store must be preserved"
    data = json.loads(state_file.read_text())
    assert "some/repo#1" in data


# ---------------------------------------------------------------------------
# AC3-UI: status reports last tick, open-PR count, parked PRs
# ---------------------------------------------------------------------------


def test_ac3ui_status_reports_last_tick_and_parked(tmp_home, tmp_launch_agents, capsys, monkeypatch):
    """status() reports last tick time, open-PR count, and parked PRs."""
    m = _install()

    # Seed a fake events.jsonl with a pr_watch_tick entry
    events_file = tmp_home / ".fno" / "events.jsonl"
    events_file.write_text(
        json.dumps({
            "type": "pr_watch_tick",
            "ts": "2026-06-14T01:00:00Z",
            "data": {"open_prs": 2, "acted": 1},
        }) + "\n"
    )

    # Seed a watermark store with one parked PR
    state_file = tmp_home / ".fno" / "pr-watcher-state.json"
    state_file.write_text(
        json.dumps({
            "owner/repo#42": {
                "parked": "retries-exhausted",
                "merge_dispatched": False,
                "last_review_ts": None,
                "retries": 3,
            }
        })
    )

    # Stub open-PR discovery to return a count of 2 (avoids real gh)
    monkeypatch.setattr(m, "_discover_open_pr_count", lambda: 2)
    # Stub launchctl list so we don't run the real binary
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: False)

    m.status(
        launch_agents_dir=tmp_launch_agents,
        events_path=events_file,
        state_path=state_file,
    )

    captured = capsys.readouterr()
    out = captured.out
    assert "2026-06-14T01:00:00Z" in out, "last tick time should appear"
    assert "2" in out, "open-PR count should appear"
    assert "owner/repo#42" in out or "42" in out, "parked PR should appear"


def test_status_json_emits_liveness_verdict(monkeypatch):
    """`pr-watch status --json` emits the liveness verdict for hooks to parse."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch._install as m

    monkeypatch.setattr(
        m, "liveness_report_live",
        lambda: {"enabled": True, "verdict": "dead", "detail": "no tick",
                 "fix": "fno pr-watch install", "loaded": True, "last_tick": None},
    )
    result = CliRunner().invoke(app, ["pr-watch", "status", "--json"])
    assert result.exit_code == 0
    payload = json.loads(result.stdout.strip())
    assert payload["verdict"] == "dead" and payload["enabled"] is True


def _settings_with_pr_watch(enabled: bool):
    class _PW:
        def __init__(self):
            self.enabled = enabled
            self.interval_seconds = 600
    class _S:
        pr_watch = _PW()
    return _S()


def test_refresh_verb_noop_when_disabled(monkeypatch):
    """`pr-watch refresh` is a no-op (never touches launchd) when disabled."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(False))
    import fno.pr_watch._install as m
    monkeypatch.setattr(m, "refresh_watcher", lambda **kw: pytest.fail("must not refresh when disabled"))

    result = CliRunner().invoke(app, ["pr-watch", "refresh"])
    assert result.exit_code == 0
    assert "disabled" in result.stdout


def test_refresh_verb_refreshes_when_enabled(monkeypatch):
    """`pr-watch refresh` calls refresh_watcher when enabled and reports the msg."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(True))
    monkeypatch.setattr(cli_mod, "_resolve_fno_binary", lambda: "/x/fno-py")
    import fno.pr_watch._install as m
    calls: list = []
    monkeypatch.setattr(m, "refresh_watcher", lambda **kw: calls.append(kw) or ("bounced x; awaiting first tick", 0))

    result = CliRunner().invoke(app, ["pr-watch", "refresh"])
    assert result.exit_code == 0
    assert len(calls) == 1
    assert calls[0]["fno_binary"] == "/x/fno-py"
    assert "pr-watch refresh:" in result.stdout


# ---------------------------------------------------------------------------
# heal: SessionStart self-heal (enabled-gated, single-flighted)
# ---------------------------------------------------------------------------


def _patch_heal_claims(monkeypatch, *, held=False):
    """Stub the claim single-flight so tests never touch the real claims root."""
    import fno.claims as claims

    acquired: list = []

    def _acquire(key, holder, **kw):
        if held:
            raise claims.ClaimHeldByOther("other-holder", pid=999, host="h", key=key)
        acquired.append(key)

    monkeypatch.setattr(claims, "acquire_claim", _acquire)
    monkeypatch.setattr(claims, "release_claim", lambda *a, **kw: None)
    return acquired


def test_heal_verb_never_installs_when_disabled(monkeypatch):
    """A never-enabled watcher is left alone (no auto-install)."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(False))
    import fno.pr_watch._install as m
    monkeypatch.setattr(m, "refresh_watcher", lambda **kw: pytest.fail("must not heal when disabled"))

    result = CliRunner().invoke(app, ["pr-watch", "heal"])
    assert result.exit_code == 0
    assert "disabled" in result.stdout


def test_heal_verb_bounces_when_enabled(monkeypatch):
    """An enabled-but-dead watcher is re-rendered + bounced, one status line."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(True))
    monkeypatch.setattr(cli_mod, "_resolve_fno_binary", lambda: "/x/fno-py")
    _patch_heal_claims(monkeypatch)
    import fno.pr_watch._install as m
    calls: list = []
    monkeypatch.setattr(m, "refresh_watcher", lambda **kw: calls.append(kw) or ("bounced; awaiting first tick", 0))

    result = CliRunner().invoke(app, ["pr-watch", "heal"])
    assert result.exit_code == 0
    assert len(calls) == 1
    assert "pr-watch heal:" in result.stdout


def test_heal_verb_single_flight_skips_when_held(monkeypatch):
    """Two concurrent SessionStarts reinstall at most once: the loser skips."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(True))
    _patch_heal_claims(monkeypatch, held=True)
    import fno.pr_watch._install as m
    monkeypatch.setattr(m, "refresh_watcher", lambda **kw: pytest.fail("loser must not heal"))

    result = CliRunner().invoke(app, ["pr-watch", "heal"])
    assert result.exit_code == 0
    assert "skipped" in result.stdout


def test_heal_verb_reports_failed_bounce(monkeypatch):
    """A wedged launchctl surfaces as a nonzero exit, never silently green."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(True))
    monkeypatch.setattr(cli_mod, "_resolve_fno_binary", lambda: "/x/fno-py")
    _patch_heal_claims(monkeypatch)
    import fno.pr_watch._install as m
    monkeypatch.setattr(m, "refresh_watcher", lambda **kw: ("bootstrap timed out", 1))

    result = CliRunner().invoke(app, ["pr-watch", "heal"])
    assert result.exit_code == 1
    assert "bootstrap timed out" in result.stdout


# ---------------------------------------------------------------------------
# Config: PrWatchBlock schema
# ---------------------------------------------------------------------------


def test_config_pr_watch_block_defaults():
    """PrWatchBlock has the specified defaults."""
    from fno.config import PrWatchBlock

    block = PrWatchBlock()
    assert block.enabled is False
    assert block.interval_seconds == 600
    assert block.retries == 3
    assert block.max_age_days == 14
    assert block.model == "claude-haiku-4-5"


def test_config_pr_watch_block_override():
    """PrWatchBlock fields can be overridden."""
    from fno.config import PrWatchBlock

    block = PrWatchBlock(enabled=True, interval_seconds=300, retries=5)
    assert block.enabled is True
    assert block.interval_seconds == 300
    assert block.retries == 5


def test_config_pr_watch_nonmapping_degrades_to_defaults():
    """config.pr_watch given a non-mapping (e.g. 42) loads as defaults, never raises."""
    from fno.config import ConfigBlock

    cb = ConfigBlock.model_validate({"pr_watch": 42})
    assert cb.pr_watch.enabled is False
    assert cb.pr_watch.interval_seconds == 600


def test_config_pr_watch_valid_mapping_overrides():
    """A valid pr_watch mapping overrides the defaults."""
    from fno.config import ConfigBlock

    cb = ConfigBlock.model_validate({"pr_watch": {"enabled": True, "interval_seconds": 120}})
    assert cb.pr_watch.enabled is True
    assert cb.pr_watch.interval_seconds == 120


def test_config_pr_watch_null_degrades_to_defaults():
    """pr_watch: null in YAML (None) degrades to defaults."""
    from fno.config import ConfigBlock

    cb = ConfigBlock.model_validate({"pr_watch": None})
    assert cb.pr_watch.enabled is False


# ---------------------------------------------------------------------------
# x-e106 AC1-HP: install activates (launchctl load) unless --no-activate
# ---------------------------------------------------------------------------


def test_install_activates_by_default(tmp_home, tmp_launch_agents, capsys, monkeypatch):
    """install(activate=True) bounces the agent and reports activation."""
    m = _install()
    monkeypatch.setattr("typer.confirm", lambda *a, **kw: True)
    calls: list[tuple] = []
    monkeypatch.setattr(
        m, "_run_launchctl_timed", lambda *a, **kw: (calls.append(a) or 0, False)
    )

    m.install(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
        dry_run=False,
        activate=True,
    )

    verbs = [a[0] for a in calls]
    assert verbs == ["bootout", "bootstrap", "kickstart"], f"got {verbs}"
    assert "Activated" in capsys.readouterr().out


def test_install_no_activate_skips_load(tmp_home, tmp_launch_agents, capsys, monkeypatch):
    """install(activate=False) writes the plist but does NOT launchctl load."""
    m = _install()
    monkeypatch.setattr("typer.confirm", lambda *a, **kw: True)
    calls: list[tuple] = []
    monkeypatch.setattr(m, "_run_launchctl", lambda *a: calls.append(a) or 0)

    m.install(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
        dry_run=False,
        activate=False,
    )

    assert not any(a and a[0] == "load" for a in calls), "--no-activate must skip load"
    out = capsys.readouterr().out
    assert "To activate" in out


def test_install_reload_bounces_when_loaded(tmp_home, tmp_launch_agents, monkeypatch):
    """A re-install boots the (possibly wedged) agent out before re-bootstrapping."""
    m = _install()
    monkeypatch.setattr("typer.confirm", lambda *a, **kw: True)
    calls: list[tuple] = []
    monkeypatch.setattr(
        m, "_run_launchctl_timed", lambda *a, **kw: (calls.append(a) or 0, False)
    )

    m.install(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
        dry_run=False,
        activate=True,
    )

    verbs = [a[0] for a in calls]
    assert verbs == ["bootout", "bootstrap", "kickstart"], f"got {verbs}"


def test_ensure_activated_rerenders_existing_plist(tmp_home, tmp_launch_agents, monkeypatch):
    """Re-enable of an existing plist re-renders it (config drift + fresh mtime)."""
    import os
    import time as _time

    m = _install()
    plist = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    plist.write_text("<plist/>")  # stale stub content
    old = _time.time() - 10_000
    os.utime(plist, (old, old))
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: False)
    monkeypatch.setattr(m, "_run_launchctl", lambda *a: 0)

    m.ensure_activated(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
    )

    content = plist.read_text()
    assert "sh.fno.pr-watcher" in content, "existing plist should be re-rendered, not left stale"
    assert plist.stat().st_mtime > old + 100, "re-render refreshes the plist mtime"


def test_install_activation_failure_is_loud(tmp_home, tmp_launch_agents, capsys, monkeypatch):
    """A failing bounce prints a loud WARNING but still writes the plist (AC1-ERR)."""
    m = _install()
    monkeypatch.setattr("typer.confirm", lambda *a, **kw: True)

    # bootout ok, bootstrap fails (rc=1) -> bounce reports failure, plist stays.
    def _fail_bootstrap(*a, **kw):
        return (0 if a[0] == "bootout" else 1, False)

    monkeypatch.setattr(m, "_run_launchctl_timed", _fail_bootstrap)

    m.install(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
        dry_run=False,
        activate=True,
    )

    out = capsys.readouterr().out
    assert "WARNING" in out and "activation failed" in out
    assert (tmp_launch_agents / "sh.fno.pr-watcher.plist").exists()


# ---------------------------------------------------------------------------
# x-8c3b: bounce (bootout -> bootstrap -> kickstart) cures a wedged launchd job
# ---------------------------------------------------------------------------


def _record_runner(calls, *, rc_by_verb=None, timeout_verb=None):
    """A _run_launchctl_timed stub that records calls and can inject rc/timeout."""
    rc_by_verb = rc_by_verb or {}

    def _run(*args, timeout_s=0):
        calls.append(args)
        verb = args[0]
        if verb == timeout_verb:
            return (-1, True)
        return (rc_by_verb.get(verb, 0), False)

    return _run


def test_bounce_order_is_bootout_bootstrap_kickstart(tmp_launch_agents):
    m = _install()
    calls: list[tuple] = []
    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "x.plist", uid=501,
        run=_record_runner(calls),
    )
    assert rc == 0
    assert [c[0] for c in calls] == ["bootout", "bootstrap", "kickstart"]
    assert calls[0] == ("bootout", "gui/501/sh.fno.pr-watcher")
    assert calls[1] == ("bootstrap", "gui/501", str(tmp_launch_agents / "x.plist"))
    assert calls[2] == ("kickstart", "-k", "gui/501/sh.fno.pr-watcher")


def test_bounce_tolerates_bootout_failure_when_not_loaded(tmp_launch_agents):
    """bootout returns nonzero for a not-loaded job; the bounce proceeds anyway."""
    m = _install()
    calls: list[tuple] = []
    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "x.plist", uid=501,
        run=_record_runner(calls, rc_by_verb={"bootout": 1}),
    )
    assert rc == 0  # bootout nonzero is expected, not fatal
    assert [c[0] for c in calls] == ["bootout", "bootstrap", "kickstart"]


def test_bounce_bootstrap_failure_is_reported(tmp_launch_agents):
    m = _install()
    calls: list[tuple] = []
    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "x.plist", uid=501,
        run=_record_runner(calls, rc_by_verb={"bootstrap": 5}),
        sleep=lambda _s: None,
    )
    assert rc == 1 and "bootstrap" in msg
    # A persistently-failing bootstrap is retried, then reported; never kickstarts.
    bootstrap_calls = [c[0] for c in calls if c[0] == "bootstrap"]
    assert len(bootstrap_calls) == m._BOOTSTRAP_RETRIES
    assert "kickstart" not in [c[0] for c in calls]


def test_bounce_bootstrap_retries_past_bootout_race(tmp_launch_agents):
    """`launchctl bootout` is async: a bootstrap fired too soon fails (rc=5)
    while the label is still settling. The bounce must retry and then succeed,
    not report a spurious failure (the `fno update` pr-watch refresh rc=5)."""
    m = _install()
    calls: list[tuple] = []
    # bootstrap fails once (rc=5, label still present), then succeeds.
    state = {"bootstrap_calls": 0}

    def _run(*args, timeout_s=0):
        calls.append(args)
        verb = args[0]
        if verb == "bootstrap":
            state["bootstrap_calls"] += 1
            return (0, False) if state["bootstrap_calls"] >= 2 else (5, False)
        return (0, False)

    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "x.plist", uid=501,
        run=_run, sleep=lambda _s: None,
    )
    assert rc == 0, msg
    assert state["bootstrap_calls"] == 2  # failed once, retried, succeeded
    assert [c[0] for c in calls] == ["bootout", "bootstrap", "bootstrap", "kickstart"]


def test_bounce_kickstart_hang_names_the_wedged_step(tmp_launch_agents):
    """A HANG (not a nonzero rc) on kickstart is fatal and names the step."""
    m = _install()
    calls: list[tuple] = []
    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "x.plist", uid=501,
        run=_record_runner(calls, timeout_verb="kickstart"),
    )
    assert rc == 1 and "kickstart" in msg and "timed out" in msg


def test_bounce_bootout_hang_is_fatal(tmp_launch_agents):
    """Even bootout, whose nonzero rc is tolerated, is fatal on a HANG."""
    m = _install()
    calls: list[tuple] = []
    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "x.plist", uid=501,
        run=_record_runner(calls, timeout_verb="bootout"),
    )
    assert rc == 1 and "bootout" in msg and "timed out" in msg
    assert [c[0] for c in calls] == ["bootout"]  # stops at the hang


def test_refresh_watcher_rerenders_then_bounces(tmp_launch_agents):
    """refresh_watcher rewrites the plist onto the given binary, then bounces."""
    m = _install()
    calls: list[tuple] = []
    plist = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    plist.write_text("<plist/>")  # stale stub -> must be overwritten
    import fno.pr_watch._install as mod
    orig = mod._run_launchctl_timed
    mod._run_launchctl_timed = _record_runner(calls)
    try:
        msg, rc = m.refresh_watcher(
            launch_agents_dir=tmp_launch_agents,
            fno_binary="/fresh/bin/fno-py",
            install_path="/usr/bin:/bin",
        )
    finally:
        mod._run_launchctl_timed = orig
    assert rc == 0
    content = plist.read_text()
    assert "/fresh/bin/fno-py" in content, "plist re-rendered onto the fresh binary"
    assert [c[0] for c in calls] == ["bootout", "bootstrap", "kickstart"]


def test_refresh_watcher_write_failure_is_error(tmp_path, monkeypatch):
    """A plist write failure returns nonzero and never reaches the bounce."""
    m = _install()
    # A regular file where the LaunchAgents dir should be -> mkdir/write fails.
    blocker = tmp_path / "blocker"
    blocker.write_text("not a dir")
    monkeypatch.setattr(m, "bounce", lambda **kw: pytest.fail("must not bounce on write failure"))
    msg, rc = m.refresh_watcher(
        launch_agents_dir=blocker / "LaunchAgents",
        fno_binary="/x/fno-py",
        install_path="/usr/bin:/bin",
    )
    assert rc == 1 and "failed to write plist" in msg


def test_heal_watcher_missing_plist_is_error(tmp_launch_agents, monkeypatch):
    m = _install()
    monkeypatch.setattr(m, "bounce", lambda **kw: pytest.fail("must not bounce"))
    msg, rc = m.heal_watcher(launch_agents_dir=tmp_launch_agents)
    assert rc == 1 and "no plist" in msg


def test_heal_watcher_bounces_when_plist_present(tmp_launch_agents):
    m = _install()
    (tmp_launch_agents / "sh.fno.pr-watcher.plist").write_text("<plist/>")
    calls: list[tuple] = []
    monkeypatch_run = _record_runner(calls)
    # heal_watcher -> bounce uses the module default runner; stub via monkeypatch.
    import fno.pr_watch._install as mod
    orig = mod._run_launchctl_timed
    mod._run_launchctl_timed = monkeypatch_run
    try:
        msg, rc = m.heal_watcher(launch_agents_dir=tmp_launch_agents)
    finally:
        mod._run_launchctl_timed = orig
    assert rc == 0
    assert [c[0] for c in calls] == ["bootout", "bootstrap", "kickstart"]


# ---------------------------------------------------------------------------
# x-e106: ensure_activated + unload_only (config-set coupling primitives)
# ---------------------------------------------------------------------------


def test_ensure_activated_noop_when_loaded(tmp_home, tmp_launch_agents, monkeypatch):
    """ensure_activated is a no-op when the agent is already loaded."""
    m = _install()
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)
    monkeypatch.setattr(m, "_run_launchctl", lambda *a: pytest.fail("must not load"))

    assert m.ensure_activated(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
    ) == "already-running"


def test_ensure_activated_installs_and_loads(tmp_home, tmp_launch_agents, monkeypatch):
    """ensure_activated writes the plist and loads it when absent."""
    m = _install()
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: False)
    loaded: list[tuple] = []
    monkeypatch.setattr(m, "_run_launchctl", lambda *a: loaded.append(a) or 0)

    outcome = m.ensure_activated(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
    )

    assert outcome == "activated"
    assert (tmp_launch_agents / "sh.fno.pr-watcher.plist").exists()
    assert loaded and loaded[0][0] == "load"


def test_ensure_activated_reports_load_failure(tmp_home, tmp_launch_agents, monkeypatch):
    """A launchctl failure returns 'load-failed' (never raises); AC1-ERR upstream."""
    m = _install()
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: False)
    monkeypatch.setattr(m, "_run_launchctl", lambda *a: 1)

    assert m.ensure_activated(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        install_path="/usr/bin:/bin",
    ) == "load-failed"


def test_unload_only_missing_plist_is_noop(tmp_home, tmp_launch_agents):
    """unload_only on an absent plist is a clean no-op."""
    m = _install()
    assert m.unload_only(launch_agents_dir=tmp_launch_agents) == "not-installed"


def test_unload_only_unloads_loaded_agent(tmp_home, tmp_launch_agents, monkeypatch):
    """unload_only unloads a loaded agent but keeps the plist."""
    m = _install()
    plist = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    plist.write_text("<plist/>")
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)
    monkeypatch.setattr(m, "_run_launchctl", lambda *a: 0)

    assert m.unload_only(launch_agents_dir=tmp_launch_agents) == "unloaded"
    assert plist.exists(), "disable keeps the plist"


# ---------------------------------------------------------------------------
# x-e106 AC1-UI / AC1-FR: liveness verdict from tick recency (pure function)
# ---------------------------------------------------------------------------


def _live(**over):
    """liveness_report with sane defaults, overridden per test."""
    m = _install()
    base = dict(
        enabled=True,
        interval_seconds=600,
        loaded=True,
        last_tick_ts="2026-06-14T01:00:00Z",
        plist_exists=True,
        plist_mtime=0.0,
        now=0.0,
    )
    base.update(over)
    return m.liveness_report(**base)


def test_liveness_disabled_is_silent():
    assert _live(enabled=False)["verdict"] == "disabled"


def test_liveness_healthy_recent_tick():
    # tick at now (age 0) < 2x interval -> healthy
    now = _install()._parse_ts("2026-06-14T01:00:00Z")
    assert _live(now=now)["verdict"] == "healthy"


def test_liveness_dead_stale_tick():
    # tick is 3600s old, 2x interval is 1200s -> dead (AC1-UI)
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    v = _live(now=tick + 3600)
    assert v["verdict"] == "dead"
    assert "pr-watch install" in v["fix"]


def test_liveness_dead_not_loaded():
    assert _live(loaded=False)["verdict"] == "dead"


def test_liveness_dead_no_plist():
    assert _live(plist_exists=False)["verdict"] == "dead"


def test_liveness_fresh_install_no_tick_is_pending():
    # No tick yet, plist installed just now (< 2x interval) -> healthy-pending
    v = _live(last_tick_ts=None, plist_mtime=100.0, now=200.0)
    assert v["verdict"] == "healthy-pending"


def test_liveness_no_tick_old_install_is_dead():
    # No tick and plist installed long ago (> 2x interval) -> dead (AC1-FR class)
    v = _live(last_tick_ts=None, plist_mtime=0.0, now=5000.0)
    assert v["verdict"] == "dead"


def test_liveness_reenabled_plist_newer_than_old_tick_is_pending():
    # Re-enable case: an OLD tick exists, but the plist was re-rendered just now
    # (newer than the tick, within grace) -> healthy-pending, not a false dead.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    v = _live(last_tick_ts="2026-06-14T01:00:00Z", plist_mtime=tick + 5000, now=tick + 5100)
    assert v["verdict"] == "healthy-pending"


def test_liveness_old_tick_and_old_plist_still_dead():
    # Old tick AND old plist (not freshly reinstalled) -> genuinely dead.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    v = _live(last_tick_ts="2026-06-14T01:00:00Z", plist_mtime=tick, now=tick + 5000)
    assert v["verdict"] == "dead"
