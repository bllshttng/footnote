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
    # ProcessType Standard (x-c79d): the positive read is the control for the
    # negative one below.
    assert "<key>ProcessType</key>\n  <string>Standard</string>" in rendered
    assert "<string>Background</string>" not in rendered


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
        activate=False,
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
        activate=False,
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
                "last_seen_state": "OPEN",
                "merge_dispatched": False,
                "last_review_ts": None,
                "retries": 3,
            },
            "owner/repo#43": {
                "parked": None,
                "last_seen_state": "OPEN",
                "merge_dispatched": False,
                "last_review_ts": None,
                "retries": 0,
            },
        })
    )

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


def test_status_open_count_reads_observed_state_not_graph(tmp_path):
    """The operator count comes from the cache that the tick just swept."""
    from fno.pr_watch._install import _observed_open_pr_count

    state_file = tmp_path / "pr-watcher-state.json"
    state_file.write_text(json.dumps({
        "owner/one#1": {"last_seen_state": "OPEN"},
        "owner/two#2": {"last_seen_state": "UNKNOWN"},
        "owner/three#3": {"last_seen_state": "CLOSED"},
    }))

    assert _observed_open_pr_count(state_file) == 1


def test_parked_prs_includes_pending_delivery_failures(tmp_path):
    from fno.pr_watch._install import _parked_prs

    state_file = tmp_path / "pr-watcher-state.json"
    state_file.write_text("{}")
    delivery_file = tmp_path / "pr-watcher-state-delivery.json"
    delivery_file.write_text(json.dumps({
        "owner/repo#7": {"retries": 3, "parked": "retries-exhausted"}
    }))

    assert _parked_prs(state_file) == {
        "owner/repo#7 [delivery]": "retries-exhausted"
    }


def test_status_json_emits_liveness_verdict(monkeypatch):
    """`pr-watch status --json` emits the liveness verdict for hooks to parse."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.pr_watch._install as m

    monkeypatch.setattr(
        m, "liveness_report_live",
        lambda **_kw: {"enabled": True, "verdict": "dead", "detail": "no tick",
                       "fix": "fno do pr watch install", "loaded": True, "last_tick": None},
    )
    result = CliRunner().invoke(app, ["pr-watch", "status", "--json"])
    assert result.exit_code == 0
    payload = json.loads(result.stdout.strip())
    assert payload["verdict"] == "dead" and payload["enabled"] is True


# ---------------------------------------------------------------------------
# x-c12c wave 1: status prints a verdict, not a fact (AC10/AC11)
# ---------------------------------------------------------------------------


def _write_tick_events(events_file, *, tick_ts, attempt_ts=None, end=None, tick_data=None):
    lines = []
    if attempt_ts:
        lines.append({"type": "pr_watch_tick_attempt", "ts": attempt_ts,
                      "data": {"pid": 111, "phase": "entry"}})
    if tick_ts:
        lines.append({"type": "pr_watch_tick", "ts": tick_ts,
                      "data": tick_data or {
                          "open_prs": 0, "acted": 0, "swept_count": 0, "swept": {},
                          "dropped_count": 0, "dropped": {},
                      }})
    if end:
        lines.append({"type": "pr_watch_tick_end", "ts": tick_ts, "data": end})
    events_file.parent.mkdir(parents=True, exist_ok=True)
    events_file.write_text("".join(json.dumps(ln) + "\n" for ln in lines))


def test_status_prints_healthy_verdict_and_both_watermarks(
    tmp_home, tmp_launch_agents, capsys, monkeypatch
):
    """AC10-HP: a live cadence reads `Verdict: healthy (...)` plus the
    attempt/outcome watermarks, in the exact label shape the done_probes grep."""
    import time as _time
    from datetime import datetime, timezone
    import fno.pr_watch._install as m

    (tmp_home / ".fno" / "config.toml").write_text("[pr_watch]\nenabled = true\n")
    plist_path = tmp_launch_agents / m._PLIST_FILENAME
    plist_path.write_text("<plist/>")
    # Plist older than the tick: the fresh-install grace must not fire.
    old = _time.time() - 60
    import os as _os
    _os.utime(plist_path, (old, old))
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)

    now = datetime.now(timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z")
    events_file = tmp_home / ".fno" / "events.jsonl"
    _write_tick_events(
        events_file, tick_ts=now, attempt_ts=now,
        end={"outcome": "ok", "duration_s": 14.2, "phase": "catchup", "pid": 111},
    )

    m.status(launch_agents_dir=tmp_launch_agents, events_path=events_file)
    out = capsys.readouterr().out
    import re
    assert re.search(r"^Verdict: +healthy \(", out, re.M), out
    assert re.search(r"^Last tick outcome: +ok \(14\.2s\)", out, re.M), out
    assert re.search(r"^Last attempt: ", out, re.M), out


def test_status_prints_completed_tick_marker_for_valid_receipt(
    tmp_home, tmp_launch_agents, capsys, monkeypatch
):
    """AC5-HP: status exposes a positive, count-consistent completed marker."""
    import os as _os
    import re
    from datetime import datetime, timezone
    import fno.pr_watch._install as m

    (tmp_home / ".fno" / "config.toml").write_text("[pr_watch]\nenabled = true\n")
    plist_path = tmp_launch_agents / m._PLIST_FILENAME
    plist_path.write_text("<plist/>")
    old = _os.path.getmtime(plist_path) - 60
    _os.utime(plist_path, (old, old))
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)

    now = datetime.now(timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z")
    events_file = tmp_home / ".fno" / "events.jsonl"
    _write_tick_events(
        events_file,
        tick_ts=now,
        tick_data={
            "open_prs": 2,
            "acted": 0,
            "swept_count": 2,
            "swept": {"owner/repo": [41, 42]},
        },
    )

    m.status(launch_agents_dir=tmp_launch_agents, events_path=events_file)
    out = capsys.readouterr().out
    assert re.search(rf"^Completed tick: +{re.escape(now)} swept=2$", out, re.M), out


def test_status_accepts_chunked_completed_tick_receipt(
    tmp_home, tmp_launch_agents, capsys, monkeypatch
):
    """Chunked receipts remain positive when the summary carries no inline map."""
    import fno.pr_watch._install as m
    from datetime import datetime, timezone
    import re

    (tmp_home / ".fno" / "config.toml").write_text("[pr_watch]\nenabled = true\n")
    plist_path = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    plist_path.write_text("plist")
    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    events_file = tmp_home / ".fno" / "events.jsonl"
    events_file.write_text("\n".join([
        json.dumps({
            "type": "pr_watch_sweep_chunk",
            "ts": now,
            "data": {
                "receipt_id": "chunked",
                "chunk_index": 1,
                "chunk_count": 1,
                "item_count": 2,
                "items": [
                    {"action": "swept", "key": "owner/repo#41"},
                    {"action": "swept", "key": "owner/repo#42"},
                ],
            },
        }),
        json.dumps({
            "type": "pr_watch_tick",
            "ts": now,
            "data": {
                "swept_count": 2,
                "swept": {},
                "receipt_id": "chunked",
                "receipt_chunks": 1,
            },
        }),
    ]) + "\n")
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)

    m.status(launch_agents_dir=tmp_launch_agents, events_path=events_file)
    out = capsys.readouterr().out

    assert re.search(rf"^Completed tick: +{re.escape(now)} swept=2$", out, re.M), out


def test_status_preserves_latest_positive_marker_after_quiet_tick(
    tmp_home, tmp_launch_agents, capsys, monkeypatch
):
    """An invalid later receipt cannot erase an earlier positive marker."""
    import fno.pr_watch._install as m
    from datetime import datetime, timezone, timedelta
    import re

    (tmp_home / ".fno" / "config.toml").write_text("[pr_watch]\nenabled = true\n")
    plist_path = tmp_launch_agents / "sh.fno.pr-watcher.plist"
    plist_path.write_text("plist")
    now = datetime.now(timezone.utc)
    first = now - timedelta(seconds=5)
    first_text = first.strftime("%Y-%m-%dT%H:%M:%SZ")
    second_text = now.strftime("%Y-%m-%dT%H:%M:%SZ")
    events_file = tmp_home / ".fno" / "events.jsonl"
    events_file.write_text("\n".join([
        json.dumps({
            "type": "pr_watch_tick",
            "ts": first_text,
            "data": {"swept_count": 1, "swept": {"owner/repo": [41]}},
        }),
        json.dumps({
            "type": "pr_watch_tick",
            "ts": second_text,
            "data": {"swept_count": 0, "swept": {}},
        }),
    ]) + "\n")
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)

    m.status(launch_agents_dir=tmp_launch_agents, events_path=events_file)
    out = capsys.readouterr().out

    assert re.search(rf"^Completed tick: +{re.escape(first_text)} swept=1$", out, re.M), out


@pytest.mark.parametrize(
    "tick_data",
    [
        {"swept_count": 0, "swept": {}},
        {"swept_count": 1, "swept": {}},
        {"swept_count": 2, "swept": {"owner/repo": [41]}},
        {"swept_count": 1, "swept": {"owner/repo": [41, 42]}},
    ],
)
def test_status_rejects_invalid_completed_tick_receipts(
    tick_data, tmp_home, tmp_launch_agents, capsys, monkeypatch
):
    """AC5-ERR: zero, empty, or inconsistent receipts cannot claim completion."""
    import os as _os
    import re
    from datetime import datetime, timezone
    import fno.pr_watch._install as m

    (tmp_home / ".fno" / "config.toml").write_text("[pr_watch]\nenabled = true\n")
    plist_path = tmp_launch_agents / m._PLIST_FILENAME
    plist_path.write_text("<plist/>")
    old = _os.path.getmtime(plist_path) - 60
    _os.utime(plist_path, (old, old))
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)

    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    events_file = tmp_home / ".fno" / "events.jsonl"
    _write_tick_events(events_file, tick_ts=now, tick_data=tick_data)

    m.status(launch_agents_dir=tmp_launch_agents, events_path=events_file)
    out = capsys.readouterr().out
    assert not re.search(r"^Completed tick: +20[0-9]{2}-.* swept=[1-9][0-9]*$", out, re.M), out


def test_status_prints_dead_verdict_with_fix_command(
    tmp_home, tmp_launch_agents, capsys, monkeypatch
):
    """AC10-HP: a stale cadence reads `dead` with the detail and the fix verb."""
    import time as _time
    from datetime import datetime, timezone, timedelta
    import fno.pr_watch._install as m

    (tmp_home / ".fno" / "config.toml").write_text("[pr_watch]\nenabled = true\n")
    plist_path = tmp_launch_agents / m._PLIST_FILENAME
    plist_path.write_text("<plist/>")
    stale = _time.time() - 7200
    import os as _os
    _os.utime(plist_path, (stale, stale))
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)

    old_ts = (datetime.now(timezone.utc) - timedelta(hours=3)).strftime("%Y-%m-%dT%H:%M:%SZ")
    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    events_file = tmp_home / ".fno" / "events.jsonl"
    # Attempts advancing while Last tick stands still: the exact fault profile.
    _write_tick_events(
        events_file, tick_ts=old_ts, attempt_ts=now,
        end={"outcome": "timeout", "duration_s": 480.0, "phase": "sweep", "pid": 111},
    )

    m.status(launch_agents_dir=tmp_launch_agents, events_path=events_file)
    out = capsys.readouterr().out
    import re
    assert re.search(r"^Verdict: +dead \(last tick \d+s ago", out, re.M), out
    assert re.search(r"^Fix: +fno do pr watch install", out, re.M), out
    assert "timeout" in out


def test_status_prints_disabled_verdict_by_default(
    tmp_home, tmp_launch_agents, capsys, monkeypatch
):
    """AC10-EDGE: the third verdict, disabled, is named rather than implied."""
    import fno.pr_watch._install as m

    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: False)
    m.status(launch_agents_dir=tmp_launch_agents)
    out = capsys.readouterr().out
    assert "Verdict:      disabled (" in out, out


class _SpyEventsPath:
    """Count read_text calls: the one-pass scan proof (AC11)."""

    def __init__(self, path):
        self._path = path
        self.reads = 0

    def exists(self):
        return self._path.exists()

    def read_text(self, *a, **kw):
        self.reads += 1
        return self._path.read_text(*a, **kw)


def test_tick_watermarks_single_pass(tmp_path):
    """AC11-EDGE: all three watermarks come from ONE read of events.jsonl."""
    from fno.pr_watch._install import _tick_watermarks

    events_file = tmp_path / "events.jsonl"
    _write_tick_events(
        events_file, tick_ts="2026-08-17T06:12:01Z", attempt_ts="2026-08-17T06:12:00Z",
        end={"outcome": "ok", "duration_s": 1.0, "phase": "catchup", "sweep_failures": 0},
    )
    spy = _SpyEventsPath(events_file)

    marks = _tick_watermarks(spy)

    assert marks["last_tick"] == "2026-08-17T06:12:01Z"
    assert marks["last_attempt"] == "2026-08-17T06:12:00Z"
    assert marks["last_end"]["outcome"] == "ok"
    assert spy.reads == 1


def test_saturated_phases_reach_the_status_bits(tmp_path):
    """The saturated marker survives the watermark pass and reads as one
    status bit; a tick with nothing saturated carries no bit."""
    from fno.pr_watch._install import _tick_watermarks, tick_end_bits

    events_file = tmp_path / "events.jsonl"
    _write_tick_events(
        events_file, tick_ts="2026-08-17T06:12:01Z", attempt_ts="2026-08-17T06:12:00Z",
        end={"outcome": "timeout", "duration_s": 479.4, "phase": "sweep",
             "sweep_failures": 0, "why": "deadline_exceeded",
             "saturated": ["sweep", "king_wake"]},
    )

    marks = _tick_watermarks(events_file)

    assert marks["last_end"]["saturated"] == ["sweep", "king_wake"]
    bits = tick_end_bits(marks["last_end"])
    assert "saturated: sweep, king_wake" in bits, bits
    assert not [b for b in tick_end_bits(
        {"outcome": "ok", "duration_s": 1.0, "phase": "catchup", "sweep_failures": 0}
    ) if b.startswith("saturated")]
    assert tick_end_bits({"saturated": []}) == []


def test_status_reads_the_event_log_once(tmp_home, tmp_launch_agents, capsys, monkeypatch):
    """AC11-EDGE at the status boundary: the verdict reuses the marks already
    read; a second scan would double the read count."""
    from datetime import datetime, timezone
    import fno.pr_watch._install as m

    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: False)
    events_file = tmp_home / ".fno" / "events.jsonl"
    now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    _write_tick_events(events_file, tick_ts=now, attempt_ts=now)
    spy = _SpyEventsPath(events_file)

    m.status(launch_agents_dir=tmp_launch_agents, events_path=spy)
    capsys.readouterr()
    assert spy.reads == 1, "status must derive verdict and watermarks from one pass"


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
    touch the real claims root, event log or launchctl."""
    import fno.backlog.single_flight as sf

    acquired: list = []

    import fno.pr_watch._install as m
    if probe_raises:
        def _raise():
            raise RuntimeError("probe unavailable")
        monkeypatch.setattr(m, "liveness_report_live", _raise)
    else:
        if probe is None:
            probe = {"bounce_pending": False}
        monkeypatch.setattr(m, "liveness_report_live", lambda: probe)

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
    events = [
        json.loads(line)
        for line in (state_root / "events.jsonl").read_text().splitlines()
    ]
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
    events = [
        json.loads(line)
        for line in (state_root / "events.jsonl").read_text().splitlines()
    ]
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
    assert "do pr watch install" in v["fix"]


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


# ---------------------------------------------------------------------------
# A broken post-install tick end defeats the fresh-install grace
# ---------------------------------------------------------------------------


def _end_at(epoch, outcome="timeout", phase="recovery", duration_s=484.6):
    """A pr_watch_tick_end watermark shaped like _tick_watermarks writes it."""
    from datetime import datetime as _dt, timezone as _tz

    ts = _dt.fromtimestamp(epoch, _tz.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    return {"ts": ts, "outcome": outcome, "phase": phase,
            "duration_s": duration_s, "sweep_failures": None}


def test_liveness_broken_post_install_end_defeats_grace():
    # AC1-HP: plist 600s old and newer than the last tick (3300s old) - a
    # bare grace read - but the post-install tick ended timeout -> dead.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    plist = tick + 2700
    v = _live(plist_mtime=plist, now=tick + 3300,
              last_end=_end_at(plist + 595))
    assert v["verdict"] == "dead"


def test_liveness_benign_or_old_end_keeps_grace():
    # AC1-EDGE: ok/lock_held/quota_skip never defeat the grace; neither does
    # a broken end OLDER than the plist (it predates this install).
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    plist = tick + 2700
    now = tick + 3300
    for outcome in ("ok", "lock_held", "quota_skip"):
        v = _live(plist_mtime=plist, now=now,
                  last_end=_end_at(plist + 595, outcome=outcome))
        assert v["verdict"] == "healthy-pending", outcome
    v = _live(plist_mtime=plist, now=now, last_end=_end_at(tick + 10))
    assert v["verdict"] == "healthy-pending"


def test_liveness_malformed_last_end_keeps_grace():
    # AC1-ERR: None, not a dict, or an unparseable/absent ts -> no raise,
    # and the same verdict as today.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    plist = tick + 2700
    now = tick + 3300
    for bad in (None, "timeout", {"outcome": "timeout", "ts": "not-a-ts"},
                {"outcome": "timeout", "ts": 1789138947},
                {"outcome": "timeout"}):
        v = _live(plist_mtime=plist, now=now, last_end=bad)
        assert v["verdict"] == "healthy-pending", bad


def test_liveness_defeated_grace_no_tick_names_broken_end():
    # AC2-HP: grace defeated with no completed tick -> the dead detail names
    # the broken end and never advises a reinstall.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    plist = tick + 2700
    v = _live(last_tick_ts=None, plist_mtime=plist, now=tick + 3300,
              last_end=_end_at(plist + 595))
    assert v["verdict"] == "dead"
    assert "installed 600s ago" in v["detail"]
    assert "timeout" in v["detail"]
    assert "phase: recovery" in v["detail"]
    assert "more than 2x interval" not in v["detail"]
    assert v["fix"] == "fno agents status"


def test_liveness_no_tick_old_install_unchanged_without_last_end():
    # AC2-EDGE: no last_end -> the legacy no-tick detail and fix are unchanged.
    v = _live(last_tick_ts=None, plist_mtime=0.0, now=5000.0)
    assert v["detail"] == "no tick recorded and installed more than 2x interval (1200s) ago"
    assert v["fix"] == "fno do pr watch install"


def test_liveness_broken_end_stale_tick_detail_gets_suffix():
    # The stale-tick dead arm carries the same broken-end suffix and fix.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    plist = tick + 2700
    v = _live(plist_mtime=plist, now=tick + 3300,
              last_end=_end_at(plist + 595))
    assert v["verdict"] == "dead"
    assert v["detail"].endswith("without completing")
    assert v["fix"] == "fno agents status"


def test_liveness_live_passes_last_end_to_the_verdict(tmp_path, monkeypatch):
    # AC2-LIVE: liveness_report_live feeds marks["last_end"] into the verdict,
    # so a broken post-install end can no longer read healthy-pending.
    import os as _os
    import time as _time
    import types

    m = _install()
    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda: types.SimpleNamespace(
            pr_watch=types.SimpleNamespace(
                enabled=True, interval_seconds=600, wedged_after_ticks=3
            ),
        ),
    )
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)
    plist = tmp_path / "sh.fno.pr-watcher.plist"
    plist.write_text("<plist/>", encoding="utf-8")
    now = _time.time()
    _os.utime(plist, (now - 100, now - 100))  # fresh: inside the grace window

    report = m.liveness_report_live(
        launch_agents_dir=tmp_path,
        marks={"last_tick": None, "last_attempt": None,
               "last_end": _end_at(now - 50), "completed_tick": None},
    )
    assert report["verdict"] == "dead"
    assert report["fix"] == "fno agents status"


# ---------------------------------------------------------------------------
# A fresh watermark with consecutive broken ends reads wedged, not healthy
# ---------------------------------------------------------------------------


def test_liveness_fresh_watermark_three_broken_ends_is_wedged():
    # The sweep completes, so the watermark stays fresh, while every tick
    # still dies: recency alone read this "healthy" at 26% success. The
    # streak is what liveness was missing; the cure is a re-render, not a
    # reinstall.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    ends = [_end_at(tick - 1800), _end_at(tick - 1200), _end_at(tick - 600)]
    v = _live(now=tick, recent_ends=ends)
    assert v["verdict"] == "wedged"
    assert v["fix"] == "fno do pr watch refresh"
    assert "3" in v["detail"]


def test_liveness_one_broken_end_among_ok_stays_healthy():
    # One broken tick is transient; only a CONSECUTIVE tail bounces the job.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    ends = [_end_at(tick - 1800, outcome="timeout"),
            _end_at(tick - 1200, outcome="ok"),
            _end_at(tick - 600, outcome="ok")]
    v = _live(now=tick, recent_ends=ends)
    assert v["verdict"] == "healthy"


def test_liveness_wedged_knob_is_honored():
    # pr_watch.wedged_after_ticks lowers the threshold; 2 broken ends at a
    # knob of 2 already read wedged.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    ends = [_end_at(tick - 1200), _end_at(tick - 600)]
    v = _live(now=tick, recent_ends=ends, wedged_after_ticks=2)
    assert v["verdict"] == "wedged"
    v = _live(now=tick, recent_ends=ends, wedged_after_ticks=3)
    assert v["verdict"] == "healthy"


def test_liveness_refresh_over_broken_streak_is_not_pending():
    # A refresh over a broken streak is NOT a cure: the just-rewritten plist
    # would read healthy-pending for 2x interval while every tick still dies
    # (the 2026-09-15 wedge loop). Ends that predate it are the failures the
    # bounce was supposed to cure; only a tick that ends ok clears the verdict.
    v = _live(last_tick_ts=None, plist_mtime=100.0, now=200.0,
              recent_ends=[_end_at(50), _end_at(80), _end_at(95)])
    assert v["verdict"] == "wedged"
    assert v["fix"] == "fno agents status"
    assert v["bounce_pending"] is True
    assert "no completed tick recorded" in v["detail"]


def test_liveness_bounce_over_broken_streak_reads_wedged():
    # The 2026-09-15T00:35Z specimen: heal rewrote the plist 49s ago, after
    # the newest of 16 consecutive timeout ends, with the last completed tick
    # 12500s old. The grace window held, so status read healthy-pending over
    # three and a half hours of failed ticks.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    now = tick + 12500
    plist = now - 49
    ends = [_end_at(plist - 23 - 600 * (15 - i)) for i in range(16)]
    v = _live(interval_seconds=600, last_tick_ts="2026-06-14T01:00:00Z",
              plist_mtime=plist, now=now, last_end=ends[-1], recent_ends=ends)
    assert v["verdict"] == "wedged"
    assert v["fix"] == "fno agents status"
    assert v["bounce_pending"] is True
    assert "16 consecutive broken ticks" in v["detail"]
    assert "bounced 49s ago" in v["detail"]


def test_liveness_bounce_clears_on_first_ok_end():
    # AC3-HP: a bounce that is followed by a tick which completes ok is a real
    # recovery - healthy-pending, and bounce_pending drops.
    from datetime import datetime as _dt, timezone as _tz

    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    plist = tick + 10000
    tick_iso = _dt.fromtimestamp(plist + 300, _tz.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    ends = [_end_at(plist - 600), _end_at(plist - 300),
            _end_at(plist + 290, outcome="ok")]
    v = _live(plist_mtime=plist, now=plist + 310, last_tick_ts=tick_iso,
              last_end=ends[-1], recent_ends=ends)
    assert v["verdict"] == "healthy"
    assert v["bounce_pending"] is False


def test_liveness_not_loaded_inside_window_is_not_pending():
    # AC4-ERR: a plist rewritten but never loaded must still heal - the
    # disabled/not-loaded early returns never carry bounce_pending.
    tick = _install()._parse_ts("2026-06-14T01:00:00Z")
    v = _live(loaded=False, last_tick_ts=None, plist_mtime=tick + 5000,
              now=tick + 5010)
    assert v["verdict"] == "dead"
    assert v["bounce_pending"] is False


def test_liveness_live_passes_recent_ends_to_the_verdict(tmp_path, monkeypatch):
    # The live surface feeds marks["recent_ends"] in, so a wedged streak
    # survives the real read path, not only the pure function.
    import os as _os
    import time as _time
    import types

    m = _install()
    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda: types.SimpleNamespace(
            pr_watch=types.SimpleNamespace(
                enabled=True, interval_seconds=600, wedged_after_ticks=3
            ),
        ),
    )
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)
    plist = tmp_path / "sh.fno.pr-watcher.plist"
    plist.write_text("<plist/>", encoding="utf-8")
    now = _time.time()
    _os.utime(plist, (now - 5000, now - 5000))  # old plist: no grace arm
    from datetime import datetime as _dt, timezone as _tz

    tick_iso = _dt.fromtimestamp(now - 25, _tz.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    ends = [_end_at(now - 1800), _end_at(now - 1200), _end_at(now - 600)]

    report = m.liveness_report_live(
        launch_agents_dir=tmp_path,
        marks={"last_tick": tick_iso, "last_attempt": None,
               "last_end": ends[-1], "completed_tick": None,
               "recent_ends": ends},
    )
    assert report["verdict"] == "wedged"
    assert report["fix"] == "fno do pr watch refresh"


# ---------------------------------------------------------------------------
# The completed-merge-scan receipt on the status surface
# ---------------------------------------------------------------------------


def test_liveness_report_carries_interval_and_merge_scan(
    tmp_path, monkeypatch
):
    """status --json answers the done-probe's whole question in one read:
    interval_seconds for the freshness window, and the last tick's grant-scan
    receipt with completed/completed_at."""
    import types

    m = _install()
    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda: types.SimpleNamespace(
            pr_watch=types.SimpleNamespace(
                enabled=True, interval_seconds=600, wedged_after_ticks=3
            ),
        ),
    )
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)
    plist = tmp_path / "sh.fno.pr-watcher.plist"
    plist.write_text("<plist/>", encoding="utf-8")
    # An OLD plist: a fresh one trips the healthy-pending grace arm and the
    # verdict this test pins never appears. The tick is 25s old against the
    # real clock so the freshness arm reads healthy, not dead.
    import os as _os
    import time as _time
    from datetime import datetime as _dt, timezone as _tz

    now = _time.time()
    _os.utime(plist, (now - 5000, now - 5000))
    tick_iso = _dt.fromtimestamp(now - 25, _tz.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    report = m.liveness_report_live(
        launch_agents_dir=tmp_path,
        marks={"last_tick": tick_iso, "last_attempt": None,
               "last_end": None, "completed_tick": None,
               "merge_scan": {"completed": True, "completed_at": tick_iso}},
    )
    assert report["verdict"] == "healthy"
    assert report["interval_seconds"] == 600
    assert report["merge_scan"]["completed"] is True
    assert report["merge_scan"]["completed_at"] == tick_iso


def test_tick_watermarks_copy_scanned_into_merge_scan(tmp_path):
    """The status line renders scanned from the marks copy, so the copy must
    carry the receipt's scanned count."""
    from fno.pr_watch._install import _tick_watermarks

    events_file = tmp_path / "events.jsonl"
    _write_tick_events(
        events_file, tick_ts="2026-08-17T06:12:01Z",
        tick_data={
            "open_prs": 3, "acted": 0, "swept_count": 13, "swept": {},
            "dropped_count": 0, "dropped": {},
            "merge_scan": {"completed": True, "scanned": 13},
        },
    )

    marks = _tick_watermarks(events_file)

    assert marks["merge_scan"]["scanned"] == 13


def test_tick_watermarks_scanned_none_without_the_field(tmp_path):
    """A receipt from a binary older than the scanned field renders as None,
    the same honest absence the pre-merge_scan key gap gives."""
    from fno.pr_watch._install import _tick_watermarks

    events_file = tmp_path / "events.jsonl"
    _write_tick_events(
        events_file, tick_ts="2026-08-17T06:12:01Z",
        tick_data={
            "open_prs": 3, "acted": 0, "swept_count": 13, "swept": {},
            "dropped_count": 0, "dropped": {},
            "merge_scan": {"completed": True},
        },
    )

    marks = _tick_watermarks(events_file)

    assert marks["merge_scan"]["scanned"] is None


def test_status_prints_scanned_in_merge_scan_line(
    tmp_home, tmp_launch_agents, capsys, monkeypatch
):
    """Given a pr_watch_tick receipt whose merge_scan.scanned is 13, status
    renders the line with scanned=13 (was hardcoded None before)."""
    import os as _os
    import re
    from datetime import datetime, timezone
    import fno.pr_watch._install as m

    (tmp_home / ".fno" / "config.toml").write_text("[pr_watch]\nenabled = true\n")
    plist_path = tmp_launch_agents / m._PLIST_FILENAME
    plist_path.write_text("<plist/>")
    old = _os.path.getmtime(plist_path) - 60
    _os.utime(plist_path, (old, old))
    monkeypatch.setattr(m, "_launchctl_is_loaded", lambda: True)

    now = datetime.now(timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z")
    events_file = tmp_home / ".fno" / "events.jsonl"
    _write_tick_events(
        events_file, tick_ts=now,
        tick_data={
            "open_prs": 3, "acted": 0, "swept_count": 13, "swept": {},
            "dropped_count": 0, "dropped": {},
            "merge_scan": {"completed": True, "scanned": 13},
        },
    )

    m.status(launch_agents_dir=tmp_launch_agents, events_path=events_file)
    out = capsys.readouterr().out
    assert re.search(r"^Merge scan: +.*scanned=13", out, re.M), out
