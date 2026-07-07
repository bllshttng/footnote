"""PR-state watcher: plist render, gated install, uninstall, and status.

All logic lives here so ``cli.py`` stays thin and this module is independently
testable without invoking Typer machinery.

The global LaunchAgent (``sh.fno.pr-watcher``) polls ``~/.fno/graph.json`` for
open-PR backlog nodes and fires headless ``/fno:pr check`` or ``/fno:pr merged``
via ``fno pr-watch tick``.  ONE agent globally -- no per-repo plists.

Design constraints (locked):
  - NO ANTHROPIC_API_KEY in EnvironmentVariables (auth via macOS keychain OAuth)
  - RunAtLoad = false (human gate: operator runs `launchctl load` themselves)
  - ProcessType = Background
  - PATH captured at install time so launchd's minimal PATH can resolve fno/gh/claude
"""
from __future__ import annotations

import json
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import typer

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

_LABEL = "sh.fno.pr-watcher"
_PLIST_FILENAME = f"{_LABEL}.plist"
_LAUNCH_AGENTS_DIR = Path.home() / "Library" / "LaunchAgents"

_PLIST_TEMPLATE = """\
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!--
  Global PR-state watcher LaunchAgent.  ONE agent polls ~/.fno/graph.json
  for open-PR backlog nodes and fires /fno:pr check or /fno:pr merged.
  RunAtLoad is false: review the rendered plist and run
    launchctl load {plist_path}
  yourself (human gate).
-->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>

  <key>ProgramArguments</key>
  <array>
    <string>{fno_binary}</string>
    <string>pr-watch</string>
    <string>tick</string>
  </array>

  <!-- launchd launches with a minimal PATH.  Capture install-time PATH so
       gh / claude / uv are resolvable without a login shell. -->
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
    <key>HOME</key>
    <string>{home}</string>
  </dict>

  <!-- Poll every N seconds.  Default 600 (10 min). -->
  <key>StartInterval</key>
  <integer>{interval}</integer>

  <!-- Do NOT fire on agent load; wait for the first StartInterval. -->
  <key>RunAtLoad</key>
  <false/>

  <key>ProcessType</key>
  <string>Background</string>

  <!-- Belt-and-suspenders: set cwd to $HOME so any code that constructs a
       relative path at least lands somewhere writable rather than in /.
       The primary fix is that _emit_event now anchors to state_dir()
       explicitly, but WorkingDirectory is a cheap additional safety net. -->
  <key>WorkingDirectory</key>
  <string>{home}</string>

  <key>StandardOutPath</key>
  <string>{log_out}</string>

  <key>StandardErrorPath</key>
  <string>{log_err}</string>
</dict>
</plist>
"""


# ---------------------------------------------------------------------------
# XML escaping (mirrors install.sh's xml_escape)
# ---------------------------------------------------------------------------


def _xml_escape(value: str) -> str:
    """Escape characters that are illegal in XML text/attribute values."""
    return (
        value
        .replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
        .replace("'", "&apos;")
    )


# ---------------------------------------------------------------------------
# PATH augmentation
# ---------------------------------------------------------------------------


def _augment_path(install_path: str) -> str:
    """Ensure ~/.local/bin and /opt/homebrew/bin are in PATH."""
    entries = [p for p in install_path.split(":") if p]
    extras = [
        str(Path.home() / ".local" / "bin"),
        "/opt/homebrew/bin",
    ]
    for extra in extras:
        if extra not in entries:
            entries.append(extra)
    return ":".join(entries)


# ---------------------------------------------------------------------------
# render_plist
# ---------------------------------------------------------------------------


def render_plist(
    *,
    launch_agents_dir: Path,
    fno_binary: str,
    install_path: str,
    interval: int = 600,
) -> str:
    """Render the plist XML string.  No filesystem writes.

    Parameters
    ----------
    launch_agents_dir:
        The LaunchAgents directory (used to build the plist path comment).
    fno_binary:
        Absolute path to the ``fno`` binary captured at install time.
    install_path:
        The ``$PATH`` string at install time; augmented before writing.
    interval:
        ``StartInterval`` in seconds (from ``config.pr_watch.interval_seconds``).
    """
    home = str(Path.home())
    fno_state = Path(home) / ".fno"
    log_out = str(fno_state / "pr-watcher.out.log")
    log_err = str(fno_state / "pr-watcher.err.log")

    augmented_path = _augment_path(install_path)

    return _PLIST_TEMPLATE.format(
        label=_xml_escape(_LABEL),
        fno_binary=_xml_escape(fno_binary),
        path=_xml_escape(augmented_path),
        home=_xml_escape(home),
        interval=interval,
        log_out=_xml_escape(log_out),
        log_err=_xml_escape(log_err),
        plist_path=_xml_escape(str(launch_agents_dir / _PLIST_FILENAME)),
    )


# ---------------------------------------------------------------------------
# launchctl helpers (stubbed in tests via monkeypatch)
# ---------------------------------------------------------------------------


def _run_launchctl(*args: str) -> int:
    """Run launchctl; return exit code.  Best-effort: never raises."""
    try:
        result = subprocess.run(
            ["launchctl", *args],
            capture_output=True,
            text=True,
            check=False,
        )
        return result.returncode
    except OSError:
        return -1


def _launchctl_is_loaded() -> bool:
    """Return True when sh.fno.pr-watcher appears in launchctl list output."""
    try:
        result = subprocess.run(
            ["launchctl", "list"],
            capture_output=True,
            text=True,
            check=False,
        )
        return _LABEL in (result.stdout or "")
    except OSError:
        return False


# ---------------------------------------------------------------------------
# Open-PR count for status (stubbed in tests)
# ---------------------------------------------------------------------------


def _discover_open_pr_count() -> int:
    """Return the number of open-PR candidates from the live graph.  Best-effort."""
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json
        from fno.pr_watch._discover import discover_open_prs

        gpath = graph_json()
        if not gpath.exists():
            return 0
        entries = read_graph(gpath)
        return len(discover_open_prs(entries))
    except Exception:
        return 0


# ---------------------------------------------------------------------------
# install
# ---------------------------------------------------------------------------


def install(
    *,
    launch_agents_dir: Path,
    fno_binary: str,
    install_path: str,
    interval: int = 600,
    dry_run: bool = False,
    activate: bool = True,
) -> None:
    """Render the plist, print it, then gate on human confirmation before writing.

    Parameters
    ----------
    launch_agents_dir:
        Where to write ``sh.fno.pr-watcher.plist``.
    fno_binary:
        Absolute path to the ``fno`` binary.
    install_path:
        ``$PATH`` at install time.
    interval:
        Poll interval in seconds.
    dry_run:
        Print plist and hint, write nothing, do not prompt.
    activate:
        After writing the plist, run ``launchctl load`` so enabled means
        running (x-e106). ``--no-activate`` (activate=False) restores the old
        write-only behavior for packaging/CI contexts.
    """
    plist_text = render_plist(
        launch_agents_dir=launch_agents_dir,
        fno_binary=fno_binary,
        install_path=install_path,
        interval=interval,
    )

    plist_path = launch_agents_dir / _PLIST_FILENAME

    typer.echo("--- Rendered plist ---")
    typer.echo(plist_text)

    if dry_run:
        typer.echo(f"[dry-run] Would write to: {plist_path}")
        typer.echo(f"[dry-run] Then run: launchctl load {plist_path}")
        typer.echo("[dry-run] Nothing written.")
        return

    if not typer.confirm(f"Write plist to {plist_path}?"):
        typer.echo("Not installed.")
        raise SystemExit(1)

    launch_agents_dir.mkdir(parents=True, exist_ok=True)
    plist_path.write_text(plist_text, encoding="utf-8")
    typer.echo(f"Written: {plist_path}")

    if activate:
        rc = _run_launchctl("load", str(plist_path))
        if rc == 0:
            typer.echo(f"Activated: launchctl load {plist_path}")
        else:
            # Loud, never silent: SIP/headless contexts can refuse launchctl.
            # The plist is written; doctor's liveness line is the residual guard.
            typer.echo(
                f"WARNING: launchctl load failed (rc={rc}); load it manually: "
                f"launchctl load {plist_path}"
            )
    else:
        typer.echo(f"To activate: launchctl load {plist_path}")

    typer.echo(
        "Note: any per-repo scripts/post-merge/ watcher can now be retired "
        "(run scripts/post-merge/uninstall.sh in each repo).  Double-fire is "
        "safe via the idempotency marker, so migration is advisory."
    )


# ---------------------------------------------------------------------------
# Activation coupling (x-e106): enabled means running
# ---------------------------------------------------------------------------


def ensure_activated(
    *,
    launch_agents_dir: Path,
    fno_binary: str,
    install_path: str,
    interval: int = 600,
) -> str:
    """Idempotently install + load the watcher.  Non-interactive, never raises.

    Called when ``pr_watch.enabled`` is set true.  Returns one of:
    ``already-running`` (loaded, no-op), ``activated`` (wrote and/or loaded),
    ``write-failed``, ``load-failed``.  A failure is reported by the caller and
    leaves config enabled so ``fno doctor`` flags the dead watcher (AC1-ERR).
    """
    plist_path = launch_agents_dir / _PLIST_FILENAME

    if _launchctl_is_loaded():
        return "already-running"

    if not plist_path.exists():
        try:
            plist_text = render_plist(
                launch_agents_dir=launch_agents_dir,
                fno_binary=fno_binary,
                install_path=install_path,
                interval=interval,
            )
            launch_agents_dir.mkdir(parents=True, exist_ok=True)
            plist_path.write_text(plist_text, encoding="utf-8")
        except OSError:
            return "write-failed"

    rc = _run_launchctl("load", str(plist_path))
    return "activated" if rc == 0 else "load-failed"


def unload_only(*, launch_agents_dir: Path) -> str:
    """Unload the agent but keep the plist (config disable path).  Idempotent.

    Returns ``not-installed`` (no plist), ``already-unloaded``, ``unloaded``,
    or ``unload-failed``.  Never raises.
    """
    plist_path = launch_agents_dir / _PLIST_FILENAME
    if not plist_path.exists():
        return "not-installed"
    if not _launchctl_is_loaded():
        return "already-unloaded"
    rc = _run_launchctl("unload", str(plist_path))
    return "unloaded" if rc == 0 else "unload-failed"


# ---------------------------------------------------------------------------
# uninstall
# ---------------------------------------------------------------------------


def uninstall(*, launch_agents_dir: Path) -> None:
    """Unload (best-effort) and remove the plist.  Preserves watermark store."""
    plist_path = launch_agents_dir / _PLIST_FILENAME

    if plist_path.exists():
        _run_launchctl("unload", str(plist_path))
        plist_path.unlink()
        typer.echo(f"Removed: {plist_path}")
    else:
        typer.echo(f"Nothing to remove: {plist_path} does not exist")

    typer.echo("Watermark store preserved (reinstall picks up existing history).")


# ---------------------------------------------------------------------------
# status
# ---------------------------------------------------------------------------


def status(
    *,
    launch_agents_dir: Path,
    events_path: Optional[Path] = None,
    state_path: Optional[Path] = None,
) -> None:
    """Print watcher status: loaded?, last tick, open-PR count, parked PRs."""
    plist_path = launch_agents_dir / _PLIST_FILENAME

    # Loaded?
    loaded = _launchctl_is_loaded()
    typer.echo(f"Agent loaded: {'yes' if loaded else 'no'}")
    typer.echo(f"Plist path:   {plist_path} ({'exists' if plist_path.exists() else 'missing'})")

    # Last tick from events.jsonl
    last_tick_ts = _last_tick_ts(events_path)
    typer.echo(f"Last tick:    {last_tick_ts or '(no tick recorded)'}")

    # Open PRs
    open_count = _discover_open_pr_count()
    typer.echo(f"Open PRs:     {open_count}")

    # Parked PRs from watermark store
    parked = _parked_prs(state_path)
    if parked:
        typer.echo(f"Parked PRs ({len(parked)}):")
        for key, reason in parked.items():
            typer.echo(f"  {key}: {reason}")
    else:
        typer.echo("Parked PRs:   none")


def _last_tick_ts(events_path: Optional[Path]) -> Optional[str]:
    """Return the ts of the most recent pr_watch_tick event, or None."""
    if events_path is None:
        # Default path
        try:
            from fno.paths import state_dir
            events_path = state_dir() / "events.jsonl"
        except Exception:
            return None

    if not events_path.exists():
        return None

    last: Optional[str] = None
    try:
        for line in events_path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
                if not isinstance(ev, dict):
                    continue
            except json.JSONDecodeError:
                continue
            if ev.get("type") == "pr_watch_tick":
                last = ev.get("ts")
    except OSError:
        return None
    return last


def _parked_prs(state_path: Optional[Path]) -> dict:
    """Return a dict of {key: park_reason} for all parked PRs."""
    if state_path is None:
        try:
            from fno.pr_watch._state import pr_watcher_state_path
            state_path = pr_watcher_state_path()
        except Exception:
            return {}

    if not state_path.exists():
        return {}

    try:
        data = json.loads(state_path.read_text(encoding="utf-8"))
        if not isinstance(data, dict):
            return {}
    except (json.JSONDecodeError, OSError):
        return {}

    return {
        key: entry.get("parked")
        for key, entry in data.items()
        if isinstance(entry, dict) and entry.get("parked")
    }


# ---------------------------------------------------------------------------
# Liveness verdict (x-e106): doctor's residual ground-truth guard
# ---------------------------------------------------------------------------


def _parse_ts(ts: Optional[str]) -> Optional[float]:
    """Parse the canonical envelope ts (``%Y-%m-%dT%H:%M:%SZ``) to epoch seconds."""
    if not ts:
        return None
    try:
        dt = datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
        return dt.timestamp()
    except (ValueError, TypeError):
        return None


def liveness_report(
    *,
    enabled: bool,
    interval_seconds: int,
    loaded: bool,
    last_tick_ts: Optional[str],
    plist_exists: bool,
    plist_mtime: Optional[float],
    now: float,
) -> dict:
    """Pure verdict: is an enabled pr-watch actually running?  (fully injectable)

    ``verdict`` is one of ``disabled | healthy | healthy-pending | dead``.
    Derives from tick recency (ground truth), not config alone (locked decision
    #4).  A freshly-installed agent with no tick yet reads ``healthy-pending``,
    not ``dead`` (AC1-UI boundary); enabled-but-not-loaded, or a stale/absent
    tick past 2x the interval, reads ``dead`` with a fix command.
    """
    threshold = 2 * max(interval_seconds, 1)

    def verdict(v: str, detail: str, fix: Optional[str] = None) -> dict:
        return {
            "enabled": enabled,
            "verdict": v,
            "detail": detail,
            "fix": fix,
            "loaded": loaded,
            "last_tick": last_tick_ts,
        }

    if not enabled:
        return verdict("disabled", "pr_watch.enabled=false")
    if not plist_exists:
        return verdict("dead", "enabled but no LaunchAgent plist installed", "fno pr-watch install")
    if not loaded:
        return verdict("dead", "plist present but agent not loaded", "fno pr-watch install")

    tick_epoch = _parse_ts(last_tick_ts)
    if tick_epoch is None:
        if plist_mtime is not None and (now - plist_mtime) < threshold:
            return verdict("healthy-pending", "installed recently; awaiting first tick")
        return verdict(
            "dead",
            f"no tick recorded and installed more than 2x interval ({threshold}s) ago",
            "fno pr-watch install",
        )

    age = now - tick_epoch
    if age > threshold:
        return verdict(
            "dead",
            f"last tick {int(age)}s ago (> 2x interval {threshold}s)",
            "fno pr-watch install",
        )
    return verdict("healthy", f"last tick {int(age)}s ago")


def liveness_report_live() -> dict:
    """Gather ground truth (config, launchd, tick, plist mtime) and judge liveness."""
    from fno.config import load_settings

    cfg = load_settings().config.pr_watch
    plist_path = _LAUNCH_AGENTS_DIR / _PLIST_FILENAME
    plist_exists = plist_path.exists()
    plist_mtime = plist_path.stat().st_mtime if plist_exists else None

    return liveness_report(
        enabled=cfg.enabled,
        interval_seconds=cfg.interval_seconds,
        loaded=_launchctl_is_loaded(),
        last_tick_ts=_last_tick_ts(None),
        plist_exists=plist_exists,
        plist_mtime=plist_mtime,
        now=time.time(),
    )
