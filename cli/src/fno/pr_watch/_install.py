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
from pathlib import Path
from typing import Optional

import typer

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

_LABEL = "sh.fno.pr-watcher"
_PLIST_FILENAME = f"{_LABEL}.plist"

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
    typer.echo(f"To activate: launchctl load {plist_path}")
    typer.echo(
        "Note: any per-repo scripts/post-merge/ watcher can now be retired "
        "(run scripts/post-merge/uninstall.sh in each repo).  Double-fire is "
        "safe via the idempotency marker, so migration is advisory."
    )


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
