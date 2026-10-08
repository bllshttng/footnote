"""Tests for fno do pr watch CLI surface and plist installer.

TDD: tests written BEFORE implementation.  Every test targets a named
acceptance criterion from the task 1.3 spec.

No real ~/Library/LaunchAgents write, no real launchctl load, no real
claude/gh.  All I/O is redirected to tmp directories.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest


@pytest.fixture(autouse=True)
def _sandbox_bounce_receipts(tmp_path, monkeypatch):
    """Bounce receipts and pr_watch_bounce events land in a tmp state dir,
    never the developer's ~/.fno."""
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path / "state")


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
    # AC4-HP: the argv carries the current verb spelling; the retired
    # pr-watch token must not appear as a bare argv string.
    assert (
        "<string>do</string>\n"
        "    <string>pr</string>\n"
        "    <string>watch</string>\n"
        "    <string>tick</string>"
    ) in rendered
    assert "<string>pr-watch</string>" not in rendered
    assert "<false/>" in rendered  # RunAtLoad false
    # AC7-HP: both launchd log paths end under the logs/ subfolder, never
    # at the top level of the state root.
    assert "/logs/pr-watcher.out.log" in rendered
    assert "/logs/pr-watcher.err.log" in rendered
    assert "/.fno/pr-watcher.out.log" not in rendered
    assert "/.fno/pr-watcher.err.log" not in rendered
    # ProcessType Standard (x-c79d): the positive read is the control for the
    # negative one below.
    assert "<key>ProcessType</key>\n  <string>Standard</string>" in rendered
    assert "<string>Background</string>" not in rendered


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
    assert calls[0]["defer_when_ticking"] is True
    assert calls[0]["caller"] == "refresh"
    assert "pr-watch refresh:" in result.stdout


def test_refresh_verb_defers_while_tick_is_in_flight(monkeypatch, tmp_path):
    """AC2-HP: a tick mid-flight defers the refresh; the verb reports the
    deferral and launchd is never touched."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    import fno.pr_watch._install as m

    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(True))
    monkeypatch.setattr(cli_mod, "_LAUNCH_AGENTS_DIR", tmp_path / "LaunchAgents")
    monkeypatch.setattr(m, "_tick_in_flight", lambda: 4242)
    calls: list = []
    monkeypatch.setattr(
        m, "_run_launchctl_timed", lambda *a, **kw: calls.append(a) or (0, False)
    )

    result = CliRunner().invoke(app, ["pr-watch", "refresh"])
    assert result.exit_code == 0
    assert "tick in flight (pid 4242)" in result.stdout
    assert "bounce deferred" in result.stdout
    assert calls == [], "a deferred refresh must run no launchctl step"


def test_refresh_verb_bounces_when_no_tick_runs(monkeypatch, tmp_path):
    """AC2-EDGE: no tick in flight, the refresh bounces as today."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    import fno.pr_watch._install as m

    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(True))
    monkeypatch.setattr(cli_mod, "_LAUNCH_AGENTS_DIR", tmp_path / "LaunchAgents")
    monkeypatch.setattr(m, "_tick_in_flight", lambda: None)
    calls: list = []
    monkeypatch.setattr(
        m, "_run_launchctl_timed", lambda *a, **kw: calls.append(a) or (0, False)
    )

    result = CliRunner().invoke(app, ["pr-watch", "refresh"])
    assert result.exit_code == 0
    assert [c[0] for c in calls] == ["bootout", "bootstrap", "kickstart"]
    assert "bounced" in result.stdout and "awaiting first tick" in result.stdout


# ---------------------------------------------------------------------------
# heal: SessionStart self-heal (enabled-gated, single-flighted)
# ---------------------------------------------------------------------------


def _patch_heal_claims(monkeypatch, *, held=False, probe=None, probe_raises=False):
    """Stub the flight-gate single-flight and the liveness probe so tests never
    touch the real claims root, event log or launchctl. The probe is the
    binary-backed helper on the cli module; its verdict shape is the verb's."""
    import fno.backlog.single_flight as sf
    import fno.pr_watch.cli as cli_mod

    acquired: list = []

    if probe_raises:
        def _raise():
            raise RuntimeError("probe unavailable")
        monkeypatch.setattr(cli_mod, "_liveness_via_binary", _raise)
    else:
        if probe is None:
            probe = {"bounce_pending": False}
        monkeypatch.setattr(cli_mod, "_liveness_via_binary", lambda: probe)

    def _acquire(key, **kw):
        acquired.append(key)
        return sf.Flight(key=key, holder="pr-watch-heal:other", held=held)

    monkeypatch.setattr(sf, "acquire_flight", _acquire)
    # The fake's release must not reach the real binary and claims root.
    monkeypatch.setattr(sf.Flight, "release", lambda self: None)
    return acquired


def test_heal_verb_skips_while_a_bounce_awaits_its_tick(monkeypatch):
    """A bounce inside the healthy-pending grace is not a wedge to cure:
    healing again re-arms the grace over the same fault. Skip quietly."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(True))
    _patch_heal_claims(
        monkeypatch,
        probe={"verdict": "wedged", "bounce_pending": True,
               "detail": "bounced 49s ago over 16 consecutive broken ticks"},
    )
    import fno.pr_watch._install as m
    monkeypatch.setattr(
        m, "refresh_watcher", lambda **kw: pytest.fail("a pending bounce must not heal")
    )

    result = CliRunner().invoke(app, ["pr-watch", "heal"])
    assert result.exit_code == 0
    assert "bounce is pending" in result.stdout
    assert "fno do pr watch refresh" in result.stdout


def test_heal_verb_heals_when_the_probe_raises(monkeypatch):
    """A probe that cannot read never blocks a cure."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(True))
    monkeypatch.setattr(cli_mod, "_resolve_fno_binary", lambda: "/x/fno-py")
    _patch_heal_claims(monkeypatch, probe_raises=True)
    import fno.pr_watch._install as m
    calls: list = []
    monkeypatch.setattr(m, "refresh_watcher", lambda **kw: calls.append(kw) or ("bounced", 0))

    result = CliRunner().invoke(app, ["pr-watch", "heal"])
    assert result.exit_code == 0
    assert len(calls) == 1
    assert calls[0]["caller"] == "heal"


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


def test_heal_verb_defers_while_tick_is_in_flight(monkeypatch):
    """AC4-HP at the verb: the SessionStart heal passes defer_when_ticking so
    the bounce cannot kill a live tick, and names itself to the receipt."""
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
    assert calls[0]["defer_when_ticking"] is True
    assert calls[0]["caller"] == "heal"


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
    not report a spurious failure (the `fno doctor update` pr-watch refresh rc=5)."""
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


def test_bounce_records_caller_sidecar_and_event(tmp_launch_agents):
    """A real-watcher bounce writes pr-watch-bounce.json naming its caller and
    emits pr_watch_bounce, so a killed tick can name the cure that killed it."""
    import time as _time

    import fno.paths

    m = _install()
    calls: list[tuple] = []
    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "x.plist", uid=501,
        run=_record_runner(calls), caller="heal",
    )
    assert rc == 0
    state_root = Path(fno.paths.state_dir())
    sidecar = json.loads((state_root / "pr-watch-bounce.json").read_text())
    assert sidecar["caller"] == "heal"
    assert sidecar["pid"] == os.getpid()
    assert sidecar["ppid"] == os.getppid()
    assert isinstance(sidecar["parent"], str)
    assert sidecar["deferred"] is False
    assert _time.time() - sidecar["ts"] < 60
    from tests._event_rows import event_rows

    events = event_rows(state_root / "events.jsonl")
    bounces = [e for e in events if e["type"] == "pr_watch_bounce"]
    assert len(bounces) == 1
    assert bounces[0]["data"]["caller"] == "heal"
    assert bounces[0]["data"]["deferred"] is False


def test_bounce_defer_emits_event_but_no_sidecar(tmp_launch_agents, monkeypatch):
    """A deferred bounce is countable (pr_watch_bounce, deferred true) but
    writes no sidecar: there is no kill to join it to."""
    import fno.paths

    m = _install()
    monkeypatch.setattr(m, "_tick_in_flight", lambda: 4242)
    calls: list[tuple] = []
    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "x.plist", uid=501,
        run=_record_runner(calls), defer_when_ticking=True, caller="refresh",
    )
    assert (msg, rc) == ("tick in flight (pid 4242); bounce deferred", 0)
    assert calls == []
    state_root = Path(fno.paths.state_dir())
    assert not (state_root / "pr-watch-bounce.json").exists()
    from tests._event_rows import event_rows

    events = event_rows(state_root / "events.jsonl")
    bounces = [e for e in events if e["type"] == "pr_watch_bounce"]
    assert len(bounces) == 1
    assert bounces[0]["data"]["deferred"] is True
    assert bounces[0]["data"]["caller"] == "refresh"


def test_bounce_foreign_label_writes_nothing(tmp_launch_agents):
    """AC3-EDGE: groom installs its own agent through bounce with a foreign
    label (kickstart=False); no sidecar and no pr_watch_bounce event land."""
    import fno.paths

    m = _install()
    calls: list[tuple] = []
    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "groom.plist", uid=501,
        label="sh.fno.groom", kickstart=False,
        run=_record_runner(calls),
    )
    assert rc == 0
    state_root = Path(fno.paths.state_dir())
    assert not (state_root / "pr-watch-bounce.json").exists()
    events_path = state_root / "events.jsonl"
    assert not events_path.exists() or "pr_watch_bounce" not in events_path.read_text()


def test_bounce_defers_while_tick_claim_is_young(tmp_launch_agents, monkeypatch):
    """AC4-HP: a tick mid-flight (live claim under one interval) makes
    the heal defer - no launchctl step runs, and the deferral is named."""
    m = _install()
    monkeypatch.setattr(m, "_tick_in_flight", lambda: 4242)
    calls: list[tuple] = []
    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "x.plist", uid=501,
        run=_record_runner(calls),
        defer_when_ticking=True,
    )
    assert (msg, rc) == ("tick in flight (pid 4242); bounce deferred", 0)
    assert calls == []


def test_bounce_proceeds_when_tick_claim_is_old(tmp_launch_agents, monkeypatch):
    """AC5-EDGE: a live claim older than one interval is a hung tick,
    so the bounce runs its bootout/bootstrap/kickstart cure as today."""
    m = _install()
    monkeypatch.setattr(m, "_tick_in_flight", lambda: None)
    calls: list[tuple] = []
    msg, rc = m.bounce(
        plist_path=tmp_launch_agents / "x.plist", uid=501,
        run=_record_runner(calls),
        defer_when_ticking=True,
    )
    assert rc == 0
    assert [c[0] for c in calls] == ["bootout", "bootstrap", "kickstart"]


def test_tick_in_flight_asks_launchd(monkeypatch):
    """launchd owns the in-flight answer: a listed PID younger than one
    StartInterval defers the cure; an old tick, a missing PID line, or an
    unread launchctl (OSError, timeout) never blocks it."""
    import subprocess as _subprocess

    m = _install()

    def _world(pid_line: str, etime: str):
        return lambda argv: pid_line if argv[0] == "launchctl" else etime

    def probe(pid_line: str, etime: str):
        return m._tick_in_flight(run=_world(pid_line, etime))

    assert probe('"PID" = 8574;', "02:02") == 8574  # 122s old: in flight
    assert probe('"PID" = 8574;', "09:59") == 8574  # 599s: still young
    assert probe('"PID" = 8574;', "10:01") is None  # 601s: hung tick, bounces
    assert probe('"PID" = 8574;', "1-02:03:04") is None
    assert probe('"PID" = 8574;', "garbage") is None
    assert probe("", "02:02") is None  # job not loaded

    def _raise(exc):
        def _run(*_a, **_kw):
            raise exc

        return _run

    monkeypatch.setattr(m.subprocess, "run", _raise(OSError("no launchctl")))
    assert m._tick_in_flight() is None
    monkeypatch.setattr(
        m.subprocess, "run", _raise(_subprocess.TimeoutExpired("launchctl", 10))
    )
    assert m._tick_in_flight() is None


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
# x-e106: unload_only (the config-set disable coupling primitive)
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# The Heal: readout line (status, install, refresh)
# ---------------------------------------------------------------------------


def test_armed_status_with_no_binary_degrades_to_a_line_that_says_so(
    tmp_home, monkeypatch
):
    """An armed machine whose binary is missing is reported, never silence."""
    from types import SimpleNamespace

    import fno.pr_watch._install as m

    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda: SimpleNamespace(auto_heal=SimpleNamespace(enabled=True)),
    )
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: None)

    line = m.heal_status_line()
    assert line.startswith("Heal: armed; readout unavailable"), line


def test_refresh_prints_the_heal_line(tmp_home, monkeypatch):
    """A fresh refresh output carries the same Heal: readout status prints."""
    from types import SimpleNamespace

    from typer.testing import CliRunner

    from fno.cli import app
    import fno.pr_watch.cli as cli_mod
    import fno.pr_watch._install as m

    monkeypatch.setattr(cli_mod, "load_settings", lambda: _settings_with_pr_watch(True))
    monkeypatch.setattr(cli_mod, "_resolve_fno_binary", lambda: "/x/fno-py")
    monkeypatch.setattr(m, "refresh_watcher", lambda **kw: ("bounced", 0))
    monkeypatch.setattr(m, "heal_status_line", lambda events_path=None: "Heal: armed; never ran")

    result = CliRunner().invoke(app, ["pr-watch", "refresh"])
    assert result.exit_code == 0
    assert "Heal: armed; never ran" in result.stdout, result.stdout


# ---------------------------------------------------------------------------
# An unchanged refresh must not re-register the agent
# (launchd posts a macOS background-activity notice on every re-registration)
# ---------------------------------------------------------------------------


def test_default_agent_path_ignores_the_caller_environment(tmp_home, monkeypatch):
    """Two callers with different PATHs derive the same launchd PATH."""
    m = _install()
    monkeypatch.setenv(
        "PATH", "/var/run/com.apple.security.cryptexd/codex.system/usr/bin"
    )
    codex_view = m.default_agent_path("/Users/x/.local/bin/fno-py")
    monkeypatch.setenv("PATH", "/Users/claude/only/bin")
    claude_view = m.default_agent_path("/Users/x/.local/bin/fno-py")

    assert codex_view == claude_view
    assert "/opt/homebrew/bin" in codex_view and "/usr/bin" in codex_view


@pytest.mark.parametrize("use_cargo_home", [False, True])
def test_default_agent_path_carries_cargo_bin(tmp_home, monkeypatch, use_cargo_home):
    """The launchd PATH carries the cargo bin dir holding fno-agents.

    The watcher tick shells out to fno-agents/fno-agents-worker, which live in
    the cargo bin dir; a PATH without it fails every tick at binary lookup.
    """
    m = _install()
    root = tmp_home / ("cargo-home" if use_cargo_home else ".cargo")
    (root / "bin").mkdir(parents=True)
    if use_cargo_home:
        monkeypatch.setenv("CARGO_HOME", str(root))
    else:
        monkeypatch.delenv("CARGO_HOME", raising=False)
    entries = m.default_agent_path("/Users/x/.local/bin/fno-py").split(":")

    assert str(root / "bin") in entries


def test_rendered_watcher_path_contains_cargo_bin(tmp_home, tmp_launch_agents, monkeypatch):
    """The rendered plist's PATH itself carries the cargo bin dir."""
    import plistlib

    m = _install()
    cargo_bin = tmp_home / ".cargo" / "bin"
    cargo_bin.mkdir(parents=True)
    monkeypatch.delenv("CARGO_HOME", raising=False)
    rendered = m.render_plist(
        launch_agents_dir=tmp_launch_agents,
        fno_binary=str(tmp_home / ".local" / "bin" / "fno-py"),
    )
    env = plistlib.loads(rendered.encode())["EnvironmentVariables"]

    assert str(cargo_bin) in env["PATH"].split(":")


def test_refresh_unchanged_skips_write_and_bounce(tmp_home, tmp_launch_agents, monkeypatch):
    m = _install()
    plist = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    plist.write_text(
        m.render_plist(
            launch_agents_dir=tmp_launch_agents,
            fno_binary="/usr/local/bin/fno",
            interval=600,
        )
    )
    bounces: list = []
    monkeypatch.setattr(m, "bounce", lambda **kw: bounces.append(kw) or ("bounced", 0))

    msg, rc = m.refresh_watcher(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        interval=600,
    )

    assert rc == 0 and "unchanged" in msg
    assert bounces == [], "an unchanged refresh must not re-register the agent"


def test_refresh_changed_reregisters(tmp_home, tmp_launch_agents, monkeypatch):
    m = _install()
    plist = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    plist.write_text("<plist>stale</plist>")
    bounces: list = []
    monkeypatch.setattr(m, "bounce", lambda **kw: bounces.append(kw) or ("bounced", 0))

    msg, rc = m.refresh_watcher(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        interval=600,
    )

    assert rc == 0 and len(bounces) == 1
    assert "stale" not in plist.read_text()


def test_refresh_force_bounce_reregisters_an_unchanged_plist(
    tmp_home, tmp_launch_agents, monkeypatch
):
    """A dead/wedged verdict needs the re-bootstrap even with identical bytes."""
    m = _install()
    plist = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    plist.write_text(
        m.render_plist(
            launch_agents_dir=tmp_launch_agents,
            fno_binary="/usr/local/bin/fno",
            interval=600,
        )
    )
    bounces: list = []
    monkeypatch.setattr(m, "bounce", lambda **kw: bounces.append(kw) or ("bounced", 0))

    msg, rc = m.refresh_watcher(
        launch_agents_dir=tmp_launch_agents,
        fno_binary="/usr/local/bin/fno",
        interval=600,
        force_bounce=True,
    )

    assert rc == 0 and len(bounces) == 1


# ---------------------------------------------------------------------------
# An arm the tick cuts must read unhealthy, not healthy.
# ---------------------------------------------------------------------------

_MERGE_CUT_ROW = {
    "ts": "2026-06-14T01:00:00Z",
    "skip_reason": "timeout",
    "detail": "deadline exceeded in phase merge at 240s at merge:execute",
    "acted": 0,
}
