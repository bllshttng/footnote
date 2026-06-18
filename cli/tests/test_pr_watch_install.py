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
