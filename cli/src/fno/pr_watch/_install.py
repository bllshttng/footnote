"""PR-state watcher: plist render, gated install, uninstall, refresh, heal.

All logic lives here so ``cli.py`` stays thin and this module is independently
testable without invoking Typer machinery.

The global LaunchAgent (``sh.fno.pr-watcher``) polls ``~/.fno/graph.json`` for
open-PR backlog nodes and fires headless ``/fno:ship pr check`` or ``/fno:ship pr merged``
via ``fno do pr watch tick``.  ONE agent globally -- no per-repo plists.

Design constraints (locked):
  - NO ANTHROPIC_API_KEY in EnvironmentVariables (auth via macOS keychain OAuth)
  - RunAtLoad = false (human gate: operator runs `launchctl load` themselves)
  - ProcessType = Standard (Background throttled the tick 15.8x slower than
    Standard at load 161-178: 103.38s against 6.54s on one A/B loop)
  - PATH from default_agent_path (fixed install dirs), never the caller's env
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
from pathlib import Path
from typing import Callable, Optional

import typer

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

_LABEL = "sh.fno.pr-watcher"
_PLIST_FILENAME = f"{_LABEL}.plist"
# Written by _record_bounce, read by cli._bounce_sender; rename both or the
# sender join breaks silently.
_BOUNCE_SIDECAR = "pr-watch-bounce.json"
_LAUNCH_AGENTS_DIR = Path.home() / "Library" / "LaunchAgents"


_PLIST_TEMPLATE = """\
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!--
  Global PR-state watcher LaunchAgent.  ONE agent polls ~/.fno/graph.json
  for open-PR backlog nodes and fires /fno:ship pr check or /fno:ship pr merged.
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
    <string>do</string>
    <string>pr</string>
    <string>watch</string>
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
  <string>Standard</string>

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


def default_agent_path(fno_binary: str = "fno") -> str:
    entries = [str(Path(fno_binary).parent)] if "/" in fno_binary else []
    # fno-agents/fno-agents-worker live in the cargo bin dir; launchd PATH
    # without it fails every tick at binary lookup (the groom agent already
    # gets it via shutil.which("fno")).
    cargo_bin = str(Path(os.environ.get("CARGO_HOME") or Path.home() / ".cargo") / "bin")
    entries += [p for p in (cargo_bin, str(Path.home() / ".local" / "bin"), "/opt/homebrew/bin",
                            "/usr/local/bin", "/usr/bin", "/bin") if p not in entries]
    return ":".join(entries)

def _write_if_changed(plist_path: Path, plist_text: str) -> bool:
    if plist_path.exists() and plist_path.read_text(encoding="utf-8") == plist_text:
        return False
    plist_path.parent.mkdir(parents=True, exist_ok=True)
    plist_path.write_text(plist_text, encoding="utf-8")
    return True


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
    install_path: Optional[str] = None,
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
    log_out = str(fno_state / "logs" / "pr-watcher.out.log")
    log_err = str(fno_state / "logs" / "pr-watcher.err.log")

    augmented_path = _augment_path(install_path or default_agent_path(fno_binary))

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


def _stdout_of(argv: list[str]) -> str:
    """stdout of ``argv``, or "" when the command is missing, hangs, or fails.

    An unread answer never blocks a cure (fail-open, like the claim read it replaced).
    """
    try:
        result = subprocess.run(
            argv, capture_output=True, text=True, check=False,
            timeout=_LAUNCHCTL_TIMEOUT_S,
        )
        return result.stdout or ""
    except Exception:  # noqa: BLE001 - OSError or timeout reads as no tick
        return ""


# `ps -o etime=` prints [[dd-]hh:]mm:ss.
_ETIME_RE = re.compile(r"^(?:(\d+)-)?(?:(\d+):)?(\d{1,2}):(\d{2})$")


def _etime_seconds(etime: str) -> Optional[int]:
    """Parse ``[[dd-]hh:]mm:ss`` from ``ps -o etime=``; None on anything else."""
    m = _ETIME_RE.match(etime.strip())
    if m is None:
        return None
    dd = int(m.group(1) or 0)
    hh = int(m.group(2) or 0)
    return ((dd * 24 + hh) * 60 + int(m.group(3))) * 60 + int(m.group(4))


def _tick_in_flight(run: Optional[Callable[[list[str]], str]] = None) -> Optional[int]:
    """PID of a tick process younger than one StartInterval (600s), else None.

    launchd owns this answer : the old cwd-routed ``pr-watch:tick``
    claim covered only the sweep phase and read free while merge or recovery ran.
    """
    run = run or _stdout_of
    m = re.search(r'"PID" = (\d+);', run(["launchctl", "list", _LABEL]))
    if m is None:
        return None
    pid = int(m.group(1))
    age = _etime_seconds(run(["ps", "-o", "etime=", "-p", str(pid)]))
    return pid if age is not None and age < 600 else None


def _record_bounce(*, caller: str, deferred: bool, state_root: Optional[Path] = None) -> None:
    """Name this bounce so the next killed tick can name its sender.

    Writes ``pr-watch-bounce.json`` in the state dir (a deferred bounce writes
    no sidecar: there is no kill to join) and emits ``pr_watch_bounce`` so
    deferrals are countable. Never raises; a receipt must not block a cure.
    """
    data: dict = {
        "caller": caller,
        "pid": os.getpid(),
        "ppid": os.getppid(),
        "parent": _stdout_of(["ps", "-o", "command=", "-p", str(os.getppid())]).strip()[:160],
        "deferred": deferred,
    }
    if not deferred:
        try:
            from fno.paths import state_dir

            sidecar = Path(state_root or state_dir()) / _BOUNCE_SIDECAR
            sidecar.parent.mkdir(parents=True, exist_ok=True)
            tmp = sidecar.with_name(sidecar.name + ".tmp")
            tmp.write_text(json.dumps({"ts": time.time(), **data}), encoding="utf-8")
            os.replace(tmp, sidecar)
        except Exception:  # noqa: BLE001 - a receipt never blocks a cure
            pass
    try:
        from fno.events import _build, append_event
        from fno.paths import state_dir as _state_dir

        append_event(
            _build("pr_watch_bounce", "daemon", data),
            (state_root or _state_dir()) / "events.jsonl",
        )
    except Exception:  # noqa: BLE001 - a receipt never blocks a cure
        pass


def bounce(
    *,
    plist_path: Path,
    label: str = _LABEL,
    uid: Optional[int] = None,
    run: Optional[Callable[..., tuple[int, bool]]] = None,
    sleep: Callable[[float], None] = time.sleep,
    timeout_s: float = _LAUNCHCTL_TIMEOUT_S,
    kickstart: bool = True,
    defer_when_ticking: bool = False,
    caller: str = "unknown",
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

    ``defer_when_ticking``: a bounce fired mid-tick SIGTERMs that very tick, so
    heal, refresh and doctor all pass the flag; a deferred refresh leaves its
    rewritten plist for the next bounce.
    """
    if uid is None:
        uid = os.getuid()
    if run is None:
        run = _run_launchctl_timed
    if defer_when_ticking and (pid := _tick_in_flight()) is not None:
        if label == _LABEL:
            _record_bounce(caller=caller, deferred=True)
        return (f"tick in flight (pid {pid}); bounce deferred", 0)
    domain = f"gui/{uid}"
    target = f"{domain}/{label}"

    # Receipt before bootout: only this sidecar joins the SIGTERM back to its sender.
    if label == _LABEL:
        _record_bounce(caller=caller, deferred=False)

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


def heal_watcher(
    *, launch_agents_dir: Path, defer_when_ticking: bool = False, caller: str = "unknown"
) -> tuple[str, int]:
    """Resolve the plist path and bounce the watcher. Doctor's --fix entrypoint.

    Returns ``(message, exit_code)``; nonzero when the plist is absent or a step wedged.
    """
    plist_path = launch_agents_dir / _PLIST_FILENAME
    if not plist_path.exists():
        return (f"no plist at {plist_path}; run `fno do pr watch install`", 1)
    return bounce(
        plist_path=plist_path, defer_when_ticking=defer_when_ticking, caller=caller
    )


def refresh_watcher(
    *,
    launch_agents_dir: Path,
    fno_binary: str,
    install_path: Optional[str] = None,
    interval: int = 600,
    defer_when_ticking: bool = False,
    caller: str = "unknown",
    force_bounce: bool = False,
) -> tuple[str, int]:
    """Re-render the plist onto the current binary, then bounce. Post-update hook.

    Unlike :func:`heal_watcher` (bounce the existing plist), this REWRITES the
    plist first so the daemon picks up the freshly-installed binary path, a
    fresh captured PATH, and a new mtime (so doctor's ``healthy-pending`` grace
    applies until the next tick instead of a transient false ``dead`` - unless
    the recent ends are a broken streak, which reads ``wedged``). Called
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
        changed = _write_if_changed(plist_path, plist_text)
    except OSError as exc:
        return (f"failed to write plist {plist_path}: {exc}", 1)
    if not changed and not force_bounce:
        return (f"plist unchanged; not re-registered ({caller})", 0)
    return bounce(
        plist_path=plist_path, defer_when_ticking=defer_when_ticking, caller=caller
    )


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


# ---------------------------------------------------------------------------
# install


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


#: : which timeout mechanism fired; the self-kill is not a budget outcome.
_WHY_PHRASES = {
    "deadline_exceeded": "deadline exceeded",
    "slice_starved": "phase slice starved",
    "self_killed": "killed mid-sync (update bounce probable)",
    "killed": "killed by a signal",
}


#: "the tick broke" - lock_held, quota_skip and disabled are benign.
_BROKEN_OUTCOMES = ("timeout", "error")

#: How many tail end records the watermark pass keeps for the wedged streak
#: (oldest first). Caps the streak any config knob can see.
_RECENT_ENDS_KEEP = 16


def tick_end_bits(end: dict) -> list[str]:
    """The parenthesised detail bits after a tick outcome: duration, sweep
    failures, the phase name only when the tick broke (timeout or error), and
    the phases that spent their whole slice.
    Shared by `fno do pr watch status` and the pr_watch_merge arm row, so the
    arm row names the phase only when the tick broke."""
    bits: list[str] = []
    if end.get("duration_s") is not None:
        bits.append(f"{end['duration_s']:.1f}s")
    if end.get("sweep_failures"):
        bits.append(f"{end['sweep_failures']} sweep failures")
    if end.get("why"):
        bits.append(_WHY_PHRASES.get(end["why"], end["why"]))
    saturated = end.get("saturated")
    if isinstance(saturated, list) and saturated:
        bits.append("saturated: " + ", ".join(str(s) for s in saturated))
    if end.get("phase") and end.get("outcome") in _BROKEN_OUTCOMES:
        bits.append(f"phase: {end['phase']}")
    return bits


#: The unarmed readout is static: the arm command is the whole answer, and
#: shelling the Rust renderer for a constant pays a spawn on every status.
_HEAL_UNARMED = (
    "Heal: unarmed (auto_heal.enabled=false; "
    "arm with: fno config set auto_heal.enabled true)"
)


def heal_status_line(events_path: Optional[Path] = None) -> str:
    """The one ``Heal:`` readout line printed by status, install and refresh.

    Rendered by ``fno-agents pr-heal --status`` (Rust owns the journal and
    pid-file reads; this side passes only the arm bit and the journal, the
    way ``_heal_phase`` already shells ``pr-heal``). Unarmed answers without
    the binary: the arm command is the whole answer. Any readout failure
    degrades to a line that says so, never silence.
    """
    try:
        from fno.config import load_settings

        settings = load_settings()
    except Exception:  # noqa: BLE001 - an unreadable config reads unarmed
        settings = None
    armed = bool(getattr(getattr(settings, "auto_heal", None), "enabled", False))
    if not armed:
        return _HEAL_UNARMED
    try:
        import subprocess

        from fno.rust_binary import resolve_binary

        binary = resolve_binary()
        if binary is None:
            raise RuntimeError("fno-agents binary not found")
        argv = [str(binary), "pr-heal", "--status", "--armed"]
        if events_path is not None:
            argv += ["--events-file", str(events_path)]
        proc = subprocess.run(
            argv, capture_output=True, text=True, check=False, timeout=15
        )
        lines = [ln for ln in proc.stdout.splitlines() if ln.startswith("Heal:")]
        if proc.returncode != 0 or not lines:
            raise RuntimeError(f"pr-heal --status exited {proc.returncode}")
        return lines[-1]
    except Exception as exc:  # noqa: BLE001 - the readout never raises
        return f"Heal: armed; readout unavailable ({exc})"
