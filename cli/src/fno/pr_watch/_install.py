"""PR-state watcher: plist render, gated install, uninstall, and status.

All logic lives here so ``cli.py`` stays thin and this module is independently
testable without invoking Typer machinery.

The global LaunchAgent (``sh.fno.pr-watcher``) polls ``~/.fno/graph.json`` for
open-PR backlog nodes and fires headless ``/fno:pr check`` or ``/fno:pr merged``
via ``fno do pr watch tick``.  ONE agent globally -- no per-repo plists.

Design constraints (locked):
  - NO ANTHROPIC_API_KEY in EnvironmentVariables (auth via macOS keychain OAuth)
  - RunAtLoad = false (human gate: operator runs `launchctl load` themselves)
  - ProcessType = Background
  - PATH captured at install time so launchd's minimal PATH can resolve fno/gh/claude
"""

from __future__ import annotations

import json
import os
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional

import typer

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

_LABEL = "sh.fno.pr-watcher"
_PLIST_FILENAME = f"{_LABEL}.plist"
_LAUNCH_AGENTS_DIR = Path.home() / "Library" / "LaunchAgents"
# Per-repo watchers from the retired scripts/post-merge/ path. Their target
# script is deleted, so a loaded job would fail under launchd forever.
_LEGACY_POSTMERGE_GLOB = "com.fno.postmerge*.plist"


def retire_legacy_postmerge_agents(launch_agents_dir: Path) -> list[str]:
    """Bootout + remove retired per-repo post-merge watcher plists.

    Best-effort and idempotent, never raises: called on every global-watcher
    install/activate so an operator who once loaded the per-repo watcher does
    not keep a launchd job firing a deleted script. Returns receipt lines.
    """
    receipts: list[str] = []
    for plist in sorted(launch_agents_dir.glob(_LEGACY_POSTMERGE_GLOB)):
        label = plist.stem
        try:
            subprocess.run(
                ["launchctl", "bootout", f"gui/{os.getuid()}/{label}"],
                capture_output=True,
                check=False,
                timeout=10,
            )
        except Exception:
            pass  # bootout of an unloaded job fails; removal below still applies
        try:
            plist.unlink()
            receipts.append(f"retired legacy post-merge watcher: {label}")
        except OSError as exc:
            receipts.append(f"could not remove legacy watcher plist {plist}: {exc}")
    return receipts


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
        value.replace("&", "&amp;")
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


# A wedged job's `launchctl kickstart` was observed to HANG indefinitely; every
# launchctl call in the bounce is timeout-guarded so a hung fix command can't be
# worse than no fix. 10s is generous for a local launchctl round-trip.
_LAUNCHCTL_TIMEOUT_S = 10.0

# `launchctl bootout` is asynchronous: it returns before launchd finishes
# removing the service from the domain, so an immediate bootstrap can race the
# still-present label and fail (rc=5). Retry the bootstrap a few times with a
# short backoff to survive that settle window.
_BOOTSTRAP_RETRIES = 4


def _run_launchctl_timed(*args: str, timeout_s: float = _LAUNCHCTL_TIMEOUT_S) -> tuple[int, bool]:
    """Run launchctl with a timeout. Returns ``(returncode, timed_out)``.

    Separate from :func:`_run_launchctl` because the un-wedge bounce needs to
    distinguish a HANG (report which step wedged, exit nonzero) from a normal
    nonzero rc (tolerated for bootout).
    """
    try:
        result = subprocess.run(
            ["launchctl", *args],
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout_s,
        )
        return result.returncode, False
    except subprocess.TimeoutExpired:
        return -1, True
    except OSError:
        return -1, False


def bounce(
    *,
    plist_path: Path,
    label: str = _LABEL,
    uid: Optional[int] = None,
    run: Optional[Callable[..., tuple[int, bool]]] = None,
    sleep: Callable[[float], None] = time.sleep,
    timeout_s: float = _LAUNCHCTL_TIMEOUT_S,
    kickstart: bool = True,
) -> tuple[str, int]:
    """bootout -> bootstrap -> kickstart to cure a wedged launchd job.

    This is the ``dead``-verdict fix: the observed wedge (job loaded, state
    ``spawn scheduled``, never spawns, `kickstart` hangs) is only curable by
    tearing the service out of its domain (`bootout`) and re-bootstrapping it.
    Idempotent: safe on a healthy job (restart) and on a not-loaded one
    (bootout failure tolerated). Every call is timeout-guarded; on a hang it
    reports the wedged step and returns a nonzero exit code. Returns
    ``(message, exit_code)``. ``run`` is injected in tests.

    ``kickstart=False`` stops after bootstrap, for a job whose tick is not a
    harmless poll. The watcher's tick is idempotent, so forcing one is free
    liveness confirmation; a job that mutates shared state on each fire would
    instead perform that work at install time, against the plist's own schedule.
    """
    if uid is None:
        uid = os.getuid()
    if run is None:
        run = _run_launchctl_timed
    domain = f"gui/{uid}"
    target = f"{domain}/{label}"

    # 1. bootout: a nonzero rc is EXPECTED when the job is not loaded, so only a
    #    hang is fatal here.
    _, timed = run("bootout", target, timeout_s=timeout_s)
    if timed:
        return (f"`launchctl bootout {target}` timed out after {timeout_s}s", 1)

    # 2. bootstrap the plist back into the GUI domain. bootout (above) is
    #    asynchronous, so a bootstrap fired immediately after can lose to the
    #    still-settling label (rc=5). Retry with a short backoff so the refresh
    #    survives that window instead of reporting a spurious failure.
    rc = -1
    for attempt in range(_BOOTSTRAP_RETRIES):
        rc, timed = run("bootstrap", domain, str(plist_path), timeout_s=timeout_s)
        if timed:
            return (f"`launchctl bootstrap {domain}` timed out after {timeout_s}s", 1)
        if rc == 0:
            break
        if attempt + 1 < _BOOTSTRAP_RETRIES:
            sleep(0.25 * (attempt + 1))
    else:
        return (f"`launchctl bootstrap {domain} {plist_path}` failed (rc={rc})", 1)

    if not kickstart:
        return (f"bootstrapped {target}; first run at its scheduled time", 0)

    # 3. kickstart -k restarts if running; forces the first run so a fresh tick
    #    confirms liveness rather than waiting a full StartInterval.
    rc, timed = run("kickstart", "-k", target, timeout_s=timeout_s)
    if timed:
        return (f"`launchctl kickstart -k {target}` timed out after {timeout_s}s", 1)
    if rc != 0:
        return (f"`launchctl kickstart -k {target}` failed (rc={rc})", 1)

    return (f"bounced {target}; awaiting first tick", 0)


def heal_watcher(*, launch_agents_dir: Path) -> tuple[str, int]:
    """Resolve the plist path and bounce the watcher. Doctor's --fix entrypoint.

    Returns ``(message, exit_code)``; nonzero when the plist is absent (nothing
    to bounce) or a bounce step wedged.
    """
    plist_path = launch_agents_dir / _PLIST_FILENAME
    if not plist_path.exists():
        return (f"no plist at {plist_path}; run `fno do pr watch install`", 1)
    return bounce(plist_path=plist_path)


def refresh_watcher(
    *,
    launch_agents_dir: Path,
    fno_binary: str,
    install_path: str,
    interval: int = 600,
) -> tuple[str, int]:
    """Re-render the plist onto the current binary, then bounce. Post-update hook.

    Unlike :func:`heal_watcher` (bounce the existing plist), this REWRITES the
    plist first so the daemon picks up the freshly-installed binary path, a
    fresh captured PATH, and a new mtime (so doctor's ``healthy-pending`` grace
    applies until the next tick instead of a transient false ``dead``). Called
    by ``fno do pr watch refresh`` at the tail of ``fno doctor update`` so an update
    leaves an enabled watcher running the new binary and un-wedges a job a
    mid-tick reinstall may have broken. Returns ``(message, exit_code)``.
    """
    plist_path = launch_agents_dir / _PLIST_FILENAME
    try:
        plist_text = render_plist(
            launch_agents_dir=launch_agents_dir,
            fno_binary=fno_binary,
            install_path=install_path,
            interval=interval,
        )
        launch_agents_dir.mkdir(parents=True, exist_ok=True)
        plist_path.write_text(plist_text, encoding="utf-8")
    except OSError as exc:
        return (f"failed to write plist {plist_path}: {exc}", 1)
    return bounce(plist_path=plist_path)


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


def _observed_open_pr_count(state_path: Optional[Path] = None) -> int:
    """Count OPEN records from the cache most recently measured by a tick."""
    if state_path is None:
        try:
            from fno.pr_watch._state import pr_watcher_state_path

            state_path = pr_watcher_state_path()
        except Exception:
            return 0
    if not state_path.exists():
        return 0
    try:
        data = json.loads(state_path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError):
        return 0
    if not isinstance(data, dict):
        return 0
    return sum(
        1
        for entry in data.values()
        if isinstance(entry, dict) and entry.get("last_seen_state") == "OPEN"
    )


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
        # bootout+bootstrap+kickstart, not load/unload: `launchctl load` cannot
        # cure the observed wedge (job loaded, `spawn scheduled`, never spawns),
        # and this is the `dead`-verdict fix command. The bounce is idempotent,
        # so a RE-install of a healthy agent just restarts it.
        msg, rc = bounce(plist_path=plist_path)
        if rc == 0:
            typer.echo(f"Activated: {msg}")
        else:
            # Loud, never silent: SIP/headless contexts can refuse launchctl.
            # The plist is written; doctor's liveness line is the residual guard.
            typer.echo(
                f"WARNING: activation failed ({msg}); load it manually: "
                f"launchctl bootstrap gui/$(id -u) {plist_path}"
            )
    else:
        typer.echo(f"To activate: launchctl bootstrap gui/$(id -u) {plist_path}")

    for receipt in retire_legacy_postmerge_agents(launch_agents_dir):
        typer.echo(receipt)
    typer.echo(
        "The canonical pr-watch daemon now owns merge detection; no per-repo "
        "watcher install is needed."
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
    retire_legacy_postmerge_agents(launch_agents_dir)

    if _launchctl_is_loaded():
        return "already-running"

    # Always (re-)render, whether the plist is absent or a re-enable of an
    # existing one. We only reach here when NOT loaded (guarded above), so
    # rewriting is safe and (a) picks up config drift - a changed
    # interval_seconds / fno_binary / PATH since the last write - and (b)
    # refreshes the plist mtime so doctor's healthy-pending grace applies until
    # the first fresh tick instead of a transient false "dead".
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
    """Print watcher status: loaded?, verdict, watermarks, open-PR count, parked PRs."""
    plist_path = launch_agents_dir / _PLIST_FILENAME

    # Loaded?
    loaded = _launchctl_is_loaded()
    typer.echo(f"Agent loaded: {'yes' if loaded else 'no'}")
    typer.echo(f"Plist path:   {plist_path} ({'exists' if plist_path.exists() else 'missing'})")

    # All three watermarks from ONE pass (AC11); the verdict reads the same
    # marks rather than re-scanning the log.
    marks = _tick_watermarks(events_path)
    report = liveness_report_live(
        events_path=events_path,
        launch_agents_dir=launch_agents_dir,
        marks=marks,
    )
    typer.echo(f"Verdict:      {report['verdict']} ({report['detail']})")
    if report.get("fix"):
        typer.echo(f"Fix:          {report['fix']}")
    typer.echo(f"Last tick:    {marks['last_tick'] or '(no tick recorded)'}")
    typer.echo(f"Last attempt: {marks['last_attempt'] or '(no attempt recorded)'}")
    end = marks["last_end"]
    if end is None:
        typer.echo("Last tick outcome: (no end record)")
    else:
        bits: list[str] = []
        if end.get("duration_s") is not None:
            bits.append(f"{end['duration_s']:.1f}s")
        if end.get("sweep_failures"):
            bits.append(f"{end['sweep_failures']} sweep failures")
        if end.get("phase") and end.get("outcome") in ("timeout", "error"):
            bits.append(f"phase: {end['phase']}")
        detail = f" ({', '.join(bits)})" if bits else ""
        typer.echo(f"Last tick outcome: {end['outcome']}{detail}")
    completed = marks.get("completed_tick")
    if completed is None:
        typer.echo("Completed tick: none")
    else:
        typer.echo(
            f"Completed tick: {completed['ts']} swept={completed['swept_count']}"
        )

    # Fleet watchdog freshness (x-55c3): a watchdog on a dead cadence never
    # fires, and its silence is indistinguishable from a healthy fleet. When
    # the lane is armed and the last sweep is older than two intervals, say so
    # LOUD - absence is never evidence, so status never reads clean here. The
    # threshold is two CONFIGURED tick intervals: a cadence slower than the
    # 600s default would read stale while its next tick is merely not due.
    try:
        from fno.agents.watchdog import lane_armed, sweep_staleness
        from fno.config import load_settings

        settings = load_settings()
        if lane_armed(settings):
            interval = settings.pr_watch.interval_seconds
            s = sweep_staleness(stale_after_s=2 * interval)
            age = "?" if s["age_s"] is None else f"{int(s['age_s']) // 60}m"
            if s["stale"]:
                typer.echo(
                    f"FLEET WATCHDOG STALE: last sweep {age} old "
                    f"(interval {interval}s, source {s['source'] or 'none'}). "
                    "A dead cadence reads as a healthy fleet. "
                    "Sweep manually: fno agents watchdog",
                    err=True,
                )
            else:
                typer.echo(
                    f"Watchdog:     fresh ({age} old, source {s['source']})"
                )
    except Exception:  # noqa: BLE001 - status never crashes on the watchdog read
        pass

    # Open PRs
    open_count = _observed_open_pr_count(state_path)
    typer.echo(f"Open PRs:     {open_count}")

    # Parked PRs from watermark store
    parked = _parked_prs(state_path)
    if parked:
        typer.echo(f"Parked PRs ({len(parked)}):")
        for key, reason in parked.items():
            typer.echo(f"  {key}: {reason}")
    else:
        typer.echo("Parked PRs:   none")


def _tick_watermarks(events_path: Optional[Path]) -> dict:
    """Derive all three tick watermarks from a single pass over events.jsonl.

    ``last_tick`` stays the liveness watermark (completed sweeps only);
    ``last_attempt`` and ``last_end`` bracket every invocation, including the
    exits that never minted a tick. One read, three marks (AC11): the old
    per-watermark scans cost one full-log pass each.
    """
    marks: dict = {
        "last_tick": None,
        "last_attempt": None,
        "last_end": None,
        "completed_tick": None,
    }
    if events_path is None:
        try:
            from fno.paths import state_dir

            events_path = state_dir() / "events.jsonl"
        except Exception:
            return marks

    if not events_path.exists():
        return marks

    chunks_by_receipt: dict[str, list[dict]] = {}
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
            etype = ev.get("type")
            if etype == "pr_watch_sweep_chunk":
                data = ev.get("data")
                if isinstance(data, dict) and isinstance(data.get("receipt_id"), str):
                    chunks_by_receipt.setdefault(data["receipt_id"], []).append(data)
            elif etype == "pr_watch_tick":
                marks["last_tick"] = ev.get("ts")
                completed = _valid_completed_tick(
                    ev.get("ts"), ev.get("data"), chunks_by_receipt
                )
                if completed is not None:
                    marks["completed_tick"] = completed
            elif etype == "pr_watch_tick_attempt":
                marks["last_attempt"] = ev.get("ts")
            elif etype == "pr_watch_tick_end":
                data = ev.get("data")
                data = data if isinstance(data, dict) else {}
                marks["last_end"] = {
                    "ts": ev.get("ts"),
                    "outcome": data.get("outcome"),
                    "phase": data.get("phase"),
                    "duration_s": data.get("duration_s"),
                    "sweep_failures": data.get("sweep_failures"),
                }
    except OSError:
        pass
    return marks


def _valid_completed_tick(
    ts: object,
    data: object,
    chunks_by_receipt: Optional[dict[str, list[dict]]] = None,
) -> Optional[dict[str, object]]:
    """Return the positive completion marker only for a coherent sweep receipt."""
    if not isinstance(ts, str) or _parse_ts(ts) is None or not isinstance(data, dict):
        return None
    swept_count = data.get("swept_count")
    swept = data.get("swept")
    if type(swept_count) is not int or swept_count <= 0 or not isinstance(swept, dict):
        return None

    if not swept:
        receipt_id = data.get("receipt_id")
        expected_chunks = data.get("receipt_chunks")
        if (
            not isinstance(receipt_id, str)
            or type(expected_chunks) is not int
            or expected_chunks <= 0
        ):
            return None
        chunks = (chunks_by_receipt or {}).get(receipt_id)
        if not chunks or len(chunks) != expected_chunks:
            return None
        rebuilt: dict[str, list[int]] = {}
        seen: set[tuple[str, int]] = set()
        for chunk in sorted(chunks, key=lambda item: item.get("chunk_index", 0)):
            items = chunk.get("items")
            if not isinstance(items, list):
                return None
            for item in items:
                if not isinstance(item, dict) or item.get("action") != "swept":
                    continue
                key = item.get("key")
                if not isinstance(key, str) or "#" not in key:
                    return None
                repo, number_text = key.rsplit("#", 1)
                if not repo or not number_text.isdigit() or int(number_text) <= 0:
                    return None
                number = int(number_text)
                identity = (repo, number)
                if identity in seen:
                    return None
                seen.add(identity)
                rebuilt.setdefault(repo, []).append(number)
        swept = rebuilt

    identities: list[tuple[str, int]] = []
    for repo, numbers in swept.items():
        if not isinstance(repo, str) or not repo or not isinstance(numbers, list):
            return None
        if any(type(number) is not int or number <= 0 for number in numbers):
            return None
        identities.extend((repo, number) for number in numbers)
    if len(identities) != swept_count or len(set(identities)) != swept_count:
        return None
    return {"ts": ts, "swept_count": swept_count}


def _parked_prs(state_path: Optional[Path]) -> dict:
    """Return observed-cache and pending-delivery parked outcomes."""
    if state_path is None:
        try:
            from fno.pr_watch._state import pr_watcher_state_path

            state_path = pr_watcher_state_path()
        except Exception:
            return {}

    from fno.pr_watch._dispatch import _delivery_state_path

    delivery_path = _delivery_state_path(state_path)
    parked = {}
    for path, label in ((state_path, ""), (delivery_path, " [delivery]")):
        if not path.exists():
            continue
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            continue
        if not isinstance(data, dict):
            continue
        parked.update(
            {
                f"{key}{label}": entry.get("parked")
                for key, entry in data.items()
                if isinstance(entry, dict) and entry.get("parked")
            }
        )
    return parked


# ---------------------------------------------------------------------------
# Liveness verdict (x-e106): doctor's residual ground-truth guard
# ---------------------------------------------------------------------------


def _parse_ts(ts: Optional[str]) -> Optional[float]:
    """Parse a canonical UTC envelope timestamp to epoch seconds."""
    if not ts:
        return None
    try:
        dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
        if dt.tzinfo is None or dt.utcoffset() != timezone.utc.utcoffset(dt):
            return None
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
        return verdict("dead", "enabled but no LaunchAgent plist installed", "fno do pr watch install")
    if not loaded:
        return verdict("dead", "plist present but agent not loaded", "fno do pr watch install")

    # A freshly (re)installed plist newer than the last tick is awaiting its
    # first post-install tick (RunAtLoad=false, so up to one interval passes
    # before it fires). Grace it regardless of whether an OLD tick predates the
    # (re)install - otherwise a re-enabled watcher reads a transient false
    # "dead" until the next tick.
    if plist_mtime is not None and (now - plist_mtime) < threshold:
        tick_epoch = _parse_ts(last_tick_ts)
        if tick_epoch is None or plist_mtime > tick_epoch:
            return verdict("healthy-pending", "installed recently; awaiting first tick")

    tick_epoch = _parse_ts(last_tick_ts)
    if tick_epoch is None:
        return verdict(
            "dead",
            f"no tick recorded and installed more than 2x interval ({threshold}s) ago",
            "fno do pr watch install",
        )

    age = now - tick_epoch
    if age > threshold:
        return verdict(
            "dead",
            f"last tick {int(age)}s ago (> 2x interval {threshold}s)",
            "fno do pr watch install",
        )
    return verdict("healthy", f"last tick {int(age)}s ago")


def liveness_report_live(
    *,
    events_path: Optional[Path] = None,
    launch_agents_dir: Optional[Path] = None,
    marks: Optional[dict] = None,
) -> dict:
    """Gather ground truth (config, launchd, tick, plist mtime) and judge liveness.

    ``status`` passes the watermarks it already read (and its injected
    launch_agents_dir) so the event log is scanned once per invocation;
    every other caller (doctor, SessionStart hook) uses the global defaults.
    """
    from fno.config import load_settings

    settings = load_settings()
    cfg = settings.pr_watch
    plist_path = (launch_agents_dir or _LAUNCH_AGENTS_DIR) / _PLIST_FILENAME
    plist_exists = plist_path.exists()
    plist_mtime = plist_path.stat().st_mtime if plist_exists else None
    last_tick_ts = (
        marks["last_tick"] if marks is not None else _tick_watermarks(events_path)["last_tick"]
    )

    report = liveness_report(
        enabled=cfg.enabled,
        interval_seconds=cfg.interval_seconds,
        loaded=_launchctl_is_loaded(),
        last_tick_ts=last_tick_ts,
        plist_exists=plist_exists,
        plist_mtime=plist_mtime,
        now=time.time(),
    )
    # Fleet watchdog freshness rides the SAME report the --json path emits:
    # the SessionStart hook and the doctor read this dict, and a sweep starved
    # while the pr_watch tick stayed healthy must read loud there too, not
    # only in the human-readable status lines.
    try:
        from fno.agents.watchdog import lane_armed, sweep_staleness

        if lane_armed(settings):
            report["watchdog"] = sweep_staleness(
                stale_after_s=2 * cfg.interval_seconds
            )
    except Exception:  # noqa: BLE001 - liveness never crashes on the watchdog read
        pass
    return report
